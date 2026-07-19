#![allow(clippy::field_reassign_with_default)]

use super::commit_msg::render_commit_msg;
use super::formula::{FormulaOptions, generate_formula, generate_formula_with_opts};

#[test]
fn test_generate_formula() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "cfgd",
            version: "1.0.0",
            description: "Declarative config management",
            license: "MIT",
        },
        &[(
            "darwin-amd64",
            "https://example.com/cfgd-1.0.0-darwin-amd64.tar.gz",
            "sha256abc",
        )],
        &super::formula::FormulaCode {
            install: "bin.install \"cfgd\"",
            test: "system \"#{bin}/cfgd\", \"--version\"",
        },
    )
    .unwrap();
    assert!(formula.contains("class Cfgd < Formula"));
    assert!(formula.contains("version \"1.0.0\""));
    assert!(formula.contains("sha256abc"));
    assert!(formula.contains("bin.install"));
}

#[test]
fn test_generate_formula_multiple_archives() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "my-tool",
            version: "2.0.0",
            description: "A tool",
            license: "Apache-2.0",
        },
        &[
            (
                "darwin-amd64",
                "https://example.com/my-tool-2.0.0-darwin-amd64.tar.gz",
                "abc123",
            ),
            (
                "linux-amd64",
                "https://example.com/my-tool-2.0.0-linux-amd64.tar.gz",
                "def456",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"my-tool\"",
            test: "system \"#{bin}/my-tool\", \"--version\"",
        },
    )
    .unwrap();
    assert!(formula.contains("class MyTool < Formula"));
    assert!(formula.contains("on_macos"));
    assert!(formula.contains("on_linux"));
    assert!(formula.contains("abc123"));
    assert!(formula.contains("def456"));
}

#[test]
fn test_generate_formula_class_name_hyphen() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "cfgd-core",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"cfgd-core\"",
            test: "system \"#{bin}/cfgd-core\", \"--version\"",
        },
    )
    .unwrap();
    assert!(formula.contains("class CfgdCore < Formula"));
}

#[test]
fn test_generate_formula_multi_arch_grouped() {
    // darwin-amd64 and darwin-arm64 must produce a single on_macos block
    // containing on_intel and on_arm sub-blocks.
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "3.0.0",
            description: "My tool",
            license: "MIT",
        },
        &[
            (
                "darwin-amd64",
                "https://example.com/mytool-darwin-amd64.tar.gz",
                "aaaa",
            ),
            (
                "darwin-arm64",
                "https://example.com/mytool-darwin-arm64.tar.gz",
                "bbbb",
            ),
            (
                "linux-amd64",
                "https://example.com/mytool-linux-amd64.tar.gz",
                "cccc",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\", \"--version\"",
        },
    )
    .unwrap();
    // There must be exactly one on_macos block wrapping both arches.
    let macos_count = formula.matches("on_macos do").count();
    assert_eq!(macos_count, 1, "expected exactly one on_macos do block");
    assert!(formula.contains("on_arm do"));
    assert!(formula.contains("on_intel do"));
    assert!(formula.contains("aaaa"));
    assert!(formula.contains("bbbb"));
    assert!(formula.contains("cccc"));
    // on_linux should still appear once.
    assert_eq!(formula.matches("on_linux do").count(), 1);
}

// -----------------------------------------------------------------------
// Deep integration tests: verify full formula structure
// -----------------------------------------------------------------------

#[test]
fn test_integration_formula_complete_structure() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "anodizer",
            version: "3.2.1",
            description: "Release automation for Rust projects",
            license: "Apache-2.0",
        },
        &[(
                "darwin-arm64",
                "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-darwin-arm64.tar.gz",
                "aabbccdd11223344",
            )],
        &super::formula::FormulaCode {
            install: "bin.install \"anodizer\"",
            test: "system \"#{bin}/anodizer\", \"--version\"",
        }).unwrap();

    // Verify class declaration (after header comments)
    assert!(
        formula.contains("class Anodizer < Formula\n"),
        "should contain class declaration"
    );

    // Verify desc field
    assert!(formula.contains("  desc \"Release automation for Rust projects\"\n"));

    // Verify homepage (no github_slug provided, so fallback is empty string)
    assert!(formula.contains("  homepage \"\"\n"));

    // Verify license
    assert!(formula.contains("  license \"Apache-2.0\"\n"));

    // Verify version
    assert!(formula.contains("  version \"3.2.1\"\n"));

    // Verify url and sha256 (single archive = flat, no on_macos block)
    assert!(formula.contains("  url \"https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-darwin-arm64.tar.gz\"\n"));
    assert!(formula.contains("  sha256 \"aabbccdd11223344\"\n"));

    // Verify install block
    assert!(formula.contains("  def install\n"));
    assert!(formula.contains("    bin.install \"anodizer\"\n"));
    assert!(formula.contains("  end\n"));

    // Verify test block
    assert!(formula.contains("  test do\n"));
    assert!(formula.contains("    system \"#{bin}/anodizer\", \"--version\"\n"));

    // Verify formula ends properly
    assert!(formula.ends_with("end\n"));

    // Verify the overall structure has exactly one class/end pair
    assert_eq!(formula.matches("class ").count(), 1);
    // The "end" count: 1 for install, 1 for test, 1 for class
    let end_lines: Vec<&str> = formula.lines().filter(|l| l.trim() == "end").collect();
    assert_eq!(
        end_lines.len(),
        3,
        "should have 3 'end' lines: install, test, class"
    );
}

#[test]
fn test_integration_formula_multi_arch_complete_structure() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "my-cli",
            version: "2.0.0",
            description: "A CLI tool",
            license: "MIT",
        },
        &[
            (
                "darwin-arm64",
                "https://example.com/my-cli-2.0.0-darwin-arm64.tar.gz",
                "sha_darwin_arm64",
            ),
            (
                "darwin-amd64",
                "https://example.com/my-cli-2.0.0-darwin-amd64.tar.gz",
                "sha_darwin_amd64",
            ),
            (
                "linux-amd64",
                "https://example.com/my-cli-2.0.0-linux-amd64.tar.gz",
                "sha_linux_amd64",
            ),
            (
                "linux-arm64",
                "https://example.com/my-cli-2.0.0-linux-arm64.tar.gz",
                "sha_linux_arm64",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"my-cli\"",
            test: "system \"#{bin}/my-cli\", \"--version\"",
        },
    )
    .unwrap();

    // Verify class name transforms hyphen to PascalCase
    assert!(formula.contains("class MyCli < Formula"));

    // Verify on_macos block with arch sub-blocks
    assert_eq!(
        formula.matches("on_macos do").count(),
        1,
        "exactly one on_macos block"
    );
    assert_eq!(
        formula.matches("on_linux do").count(),
        1,
        "exactly one on_linux block"
    );

    // Verify on_arm and on_intel are present inside macos
    assert!(formula.contains("on_arm do"), "should have on_arm block");
    assert!(
        formula.contains("on_intel do"),
        "should have on_intel block"
    );

    // Verify all 4 URLs are present
    assert!(formula.contains("sha_darwin_arm64"));
    assert!(formula.contains("sha_darwin_amd64"));
    assert!(formula.contains("sha_linux_amd64"));
    assert!(formula.contains("sha_linux_arm64"));

    // Verify indentation of arch blocks (6 spaces for url/sha256 inside arch)
    assert!(formula.contains("      url \"https://example.com/my-cli-2.0.0-darwin-arm64.tar.gz\""));
    assert!(formula.contains("      sha256 \"sha_darwin_arm64\""));

    // Per-platform install blocks (no top-level def install for multi-arch)
    assert!(
        !formula.contains("\n  def install\n"),
        "multi-arch formula should NOT have top-level def install, got:\n{}",
        formula
    );
    assert_eq!(
        formula.matches("def install").count(),
        4,
        "each of 4 arch blocks should have its own def install, got:\n{}",
        formula
    );
    // Linux blocks should use Hardware::CPU guards
    assert!(
        formula.contains("if Hardware::CPU.intel? && Hardware::CPU.is_64_bit?"),
        "linux amd64 should use Hardware::CPU guard, got:\n{}",
        formula
    );
    assert!(
        formula.contains("if Hardware::CPU.arm? && Hardware::CPU.is_64_bit?"),
        "linux arm64 should use Hardware::CPU guard, got:\n{}",
        formula
    );
    // Verify test block is still present
    assert!(formula.contains("  test do\n"));
}

#[test]
fn test_integration_formula_no_archives() {
    // Edge case: no archive entries
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "empty-tool",
            version: "0.1.0",
            description: "An empty tool",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"empty-tool\"",
            test: "system \"#{bin}/empty-tool\", \"--help\"",
        },
    )
    .unwrap();

    assert!(formula.contains("class EmptyTool < Formula"));
    assert!(formula.contains("  version \"0.1.0\""));
    // Should not contain any url/sha256 blocks
    assert!(!formula.contains("url \""));
    assert!(!formula.contains("sha256 \""));
    // But should still have install and test
    assert!(formula.contains("  def install\n"));
    assert!(formula.contains("  test do\n"));
}

/// Regression: empty
/// archive set must hard-fail with an actionable error instead of
/// silently writing a broken formula with empty url/sha256.
#[test]
fn test_integration_formula_multiline_install() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "complex-app",
            version: "1.0.0",
            description: "Complex app",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/app.tar.gz", "hash123")],
        &super::formula::FormulaCode {
            install: "bin.install \"complex-app\"\nman1.install \"complex-app.1\"",
            test: "system \"#{bin}/complex-app\", \"--version\"\nassert_match \"complex-app\", shell_output(\"#{bin}/complex-app --help\")",
        }).unwrap();

    // Verify multi-line install block with proper indentation
    assert!(formula.contains("    bin.install \"complex-app\"\n"));
    assert!(formula.contains("    man1.install \"complex-app.1\"\n"));

    // Verify multi-line test block
    assert!(formula.contains("    system \"#{bin}/complex-app\", \"--version\"\n"));
    assert!(formula.contains("    assert_match \"complex-app\","));
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_formula_multi_arch_darwin_intel_and_arm() {
    // Verify that darwin-amd64 and darwin-arm64 produce on_intel/on_arm blocks
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "myapp",
            version: "1.0.0",
            description: "My app",
            license: "MIT",
        },
        &[
            (
                "darwin-amd64",
                "https://example.com/myapp-darwin-amd64.tar.gz",
                "hash_intel",
            ),
            (
                "darwin-arm64",
                "https://example.com/myapp-darwin-arm64.tar.gz",
                "hash_arm",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"myapp\"",
            test: "system \"#{bin}/myapp\", \"--version\"",
        },
    )
    .unwrap();

    assert_eq!(formula.matches("on_macos do").count(), 1);
    assert!(formula.contains("on_intel do"));
    assert!(formula.contains("on_arm do"));
    assert!(formula.contains("hash_intel"));
    assert!(formula.contains("hash_arm"));
    // No on_linux block since no linux archives
    assert!(!formula.contains("on_linux"));
}

#[test]
fn test_formula_single_archive_no_os_blocks() {
    // A single archive entry should use flat url/sha256, no on_macos/on_linux
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "simple",
            version: "1.0.0",
            description: "Simple tool",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/simple.tar.gz", "abc123")],
        &super::formula::FormulaCode {
            install: "bin.install \"simple\"",
            test: "system \"#{bin}/simple\"",
        },
    )
    .unwrap();

    assert!(!formula.contains("on_macos"));
    assert!(!formula.contains("on_linux"));
    assert!(formula.contains("  url \"https://example.com/simple.tar.gz\""));
    assert!(formula.contains("  sha256 \"abc123\""));
}

#[test]
fn test_formula_class_name_underscores_to_pascal_case() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "my-cool-tool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"my-cool-tool\"",
            test: "system \"#{bin}/my-cool-tool\"",
        },
    )
    .unwrap();
    assert!(formula.contains("class MyCoolTool < Formula"));
}

#[test]
fn test_formula_class_name_at_sign() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "node@20",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"node\"",
            test: "system \"#{bin}/node\"",
        },
    )
    .unwrap();
    assert!(
        formula.contains("class NodeAT20 < Formula"),
        "@ should become AT in class name"
    );
}

#[test]
fn test_formula_class_name_plus_sign() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "c++check",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"cppcheck\"",
            test: "system \"#{bin}/cppcheck\"",
        },
    )
    .unwrap();
    assert!(
        formula.contains("class Cxxcheck < Formula"),
        "+ should become x in class name"
    );
}

#[test]
fn test_formula_class_name_dot_separator() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "my.tool.app",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"my.tool.app\"",
            test: "system \"#{bin}/my.tool.app\"",
        },
    )
    .unwrap();
    assert!(
        formula.contains("class MyToolApp < Formula"),
        ". should act as word separator"
    );
}

// -----------------------------------------------------------------------
// New fields: homepage, dependencies, conflicts, caveats
// -----------------------------------------------------------------------

#[test]
fn test_formula_custom_homepage() {
    let opts = FormulaOptions {
        homepage: Some("https://example.com/mytool"),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("homepage \"https://example.com/mytool\""));
    assert!(!formula.contains("https://github.com/mytool"));
}

#[test]
fn test_formula_homepage_fallback_no_slug() {
    // When no homepage and no github_slug, homepage is empty.
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
    )
    .unwrap();
    assert!(formula.contains("homepage \"\""));
}

#[test]
fn test_formula_homepage_fallback_with_github_slug() {
    // When github_slug is set, homepage falls back to owner/repo URL.
    let opts = FormulaOptions {
        github_slug: Some("myorg/mytool".to_string()),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("homepage \"https://github.com/myorg/mytool\""));
}

#[test]
fn test_formula_dependencies_global() {
    use anodizer_core::config::HomebrewDependency;
    let deps = vec![
        HomebrewDependency {
            name: "openssl".to_string(),
            os: None,
            dep_type: None,
            version: None,
        },
        HomebrewDependency {
            name: "libgit2".to_string(),
            os: None,
            dep_type: Some("optional".to_string()),
            version: None,
        },
    ];
    let opts = FormulaOptions {
        dependencies: Some(&deps),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("depends_on \"openssl\""));
    assert!(!formula.contains("\"openssl\" => :optional"));
    assert!(formula.contains("depends_on \"libgit2\" => :optional"));
}

#[test]
fn test_formula_dependencies_os_specific() {
    use anodizer_core::config::HomebrewDependency;
    let deps = vec![
        HomebrewDependency {
            name: "macos-dep".to_string(),
            os: Some("mac".to_string()),
            dep_type: None,
            version: None,
        },
        HomebrewDependency {
            name: "linux-dep".to_string(),
            os: Some("linux".to_string()),
            dep_type: None,
            version: None,
        },
    ];
    let opts = FormulaOptions {
        dependencies: Some(&deps),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    // macos dep wrapped in on_macos block
    assert!(formula.contains("on_macos do\n    depends_on \"macos-dep\""));
    // linux dep wrapped in on_linux block
    assert!(formula.contains("on_linux do\n    depends_on \"linux-dep\""));
}

#[test]
fn test_formula_dependencies_sorted_alphabetically() {
    use anodizer_core::config::HomebrewDependency;
    // Provide deps in reverse-alphabetical order; they should be sorted in output.
    let deps = vec![
        HomebrewDependency {
            name: "zlib".to_string(),
            os: None,
            dep_type: None,
            version: None,
        },
        HomebrewDependency {
            name: "autoconf".to_string(),
            os: None,
            dep_type: None,
            version: None,
        },
        HomebrewDependency {
            name: "libgit2".to_string(),
            os: None,
            dep_type: None,
            version: None,
        },
    ];
    let opts = FormulaOptions {
        dependencies: Some(&deps),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    let autoconf_pos = formula
        .find("depends_on \"autoconf\"")
        .unwrap_or_else(|| panic!("autoconf present"));
    let libgit2_pos = formula
        .find("depends_on \"libgit2\"")
        .unwrap_or_else(|| panic!("libgit2 present"));
    let zlib_pos = formula
        .find("depends_on \"zlib\"")
        .unwrap_or_else(|| panic!("zlib present"));
    assert!(
        autoconf_pos < libgit2_pos && libgit2_pos < zlib_pos,
        "dependencies should be sorted alphabetically: autoconf < libgit2 < zlib"
    );
}

#[test]
fn test_formula_conflicts() {
    use anodizer_core::config::HomebrewConflict;
    let conflicts = vec![
        HomebrewConflict::Name("old-tool".to_string()),
        HomebrewConflict::WithReason {
            name: "other-tool".to_string(),
            because: Some("both install a foo binary".to_string()),
        },
    ];
    let opts = FormulaOptions {
        conflicts: Some(&conflicts),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("conflicts_with \"old-tool\""));
    assert!(
        formula.contains("conflicts_with \"other-tool\", because: \"both install a foo binary\"")
    );
}

#[test]
fn test_formula_caveats() {
    let opts = FormulaOptions {
        caveats: Some("Run `mytool init` after installing."),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("def caveats"));
    assert!(formula.contains("Run `mytool init` after installing."));
    assert!(formula.contains("<<~EOS"));
    assert!(formula.contains("EOS"));
}

#[test]
fn test_formula_no_caveats_block_when_none() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
    )
    .unwrap();
    assert!(!formula.contains("def caveats"));
}

#[test]
fn test_formula_all_new_fields_together() {
    use anodizer_core::config::{HomebrewConflict, HomebrewDependency};
    let deps = vec![HomebrewDependency {
        name: "openssl".to_string(),
        os: None,
        dep_type: None,
        version: None,
    }];
    let conflicts = vec![HomebrewConflict::Name("old-tool".to_string())];
    let opts = FormulaOptions {
        homepage: Some("https://example.com"),
        github_slug: None,
        dependencies: Some(&deps),
        conflicts: Some(&conflicts),
        caveats: Some("Important note."),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("homepage \"https://example.com\""));
    assert!(formula.contains("depends_on \"openssl\""));
    assert!(formula.contains("conflicts_with \"old-tool\""));
    assert!(formula.contains("def caveats"));
    assert!(formula.contains("Important note."));
}

// -----------------------------------------------------------------------
// Formula name override
// -----------------------------------------------------------------------

#[test]
fn test_formula_name_override() {
    // When HomebrewConfig.name is set, the formula should use the override
    // name for the class, not the crate name.
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "my-custom-name",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        &super::formula::FormulaCode {
            install: "bin.install \"my-custom-name\"",
            test: "system \"#{bin}/my-custom-name\"",
        },
    )
    .unwrap();
    assert!(
        formula.contains("class MyCustomName < Formula"),
        "formula class name should derive from the name override"
    );
}

// -----------------------------------------------------------------------
// Custom commit message template
// -----------------------------------------------------------------------

#[test]
fn test_render_commit_msg_default() {
    let log =
        anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
    let msg = render_commit_msg(None, "mytool", "1.2.3", "formula", &log, false).unwrap();
    assert_eq!(msg, "Brew formula update for mytool version 1.2.3");
}

#[test]
fn test_render_commit_msg_custom_template() {
    let log =
        anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
    let msg = render_commit_msg(
        Some("release: {{ name }} v{{ version }}"),
        "mytool",
        "2.0.0",
        "formula",
        &log,
        false,
    )
    .unwrap();
    assert_eq!(msg, "release: mytool v2.0.0");
}

// -----------------------------------------------------------------------
// Cask tests (Q1.1 sha256/url ordering, Q1.2 generate_completions)
// -----------------------------------------------------------------------

use super::cask::{
    CaskArchEntry, CaskParams, CaskPlatformBlock, generate_cask, render_generate_completions,
};
use anodizer_core::config::HomebrewCaskGeneratedCompletions;

fn empty_cask_params<'a>(name: &'a str, version: &'a str) -> CaskParams<'a> {
    CaskParams {
        name,
        display_name: name,
        alternative_names: &[],
        version,
        sha256: "deadbeef",
        url: "https://example.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    }
}

/// Q1.1 — per-arch blocks must emit `sha256` before `url` (upstream commit
/// 87b542b). Drift back to `url`-then-`sha256` will trip this regression.
#[test]
fn test_cask_per_arch_emits_sha256_before_url() {
    let mut params = empty_cask_params("test", "0.1.0");
    params.platforms = vec![CaskPlatformBlock {
        os_block: "macos".to_string(),
        arches: vec![
            CaskArchEntry {
                arch_block: "intel".to_string(),
                url: "https://example.com/test_Darwin_x86_64.tar.gz".to_string(),
                sha256: "macintel-hash".to_string(),
            },
            CaskArchEntry {
                arch_block: "arm".to_string(),
                url: "https://example.com/test_Darwin_arm64.tar.gz".to_string(),
                sha256: "macarm-hash".to_string(),
            },
        ],
    }];
    let cask = generate_cask(&params).unwrap();

    // Each per-arch block must have sha256 before url.
    for hash in ["macintel-hash", "macarm-hash"] {
        let sha_idx = cask
            .find(&format!("sha256 \"{}\"", hash))
            .unwrap_or_else(|| panic!("sha256 line for {hash} missing\n{cask}"));
        // The matching url comes immediately after.
        let url_after = cask[sha_idx..].find("url \"").unwrap_or_else(|| {
            panic!("expected url after sha256 for {hash}\n{cask}");
        });
        // And no `url` line should precede this sha256 inside the arch block.
        let arch_start = cask[..sha_idx]
            .rfind("on_")
            .expect("arch block start not found");
        assert!(
            !cask[arch_start..sha_idx].contains("url \""),
            "url should not precede sha256 inside the per-arch block (drift to GR pre-87b542b ordering)\n{cask}"
        );
        let _ = url_after;
    }
}

/// Q1.2 — `render_generate_completions` formats the directive exactly the
/// way upstream `generateCompletionsString` does (commit bb9062f).
#[test]
fn test_render_generate_completions_full() {
    let g = HomebrewCaskGeneratedCompletions {
        executable: Some("bin/myapp".to_string()),
        args: Some(vec!["completions".to_string()]),
        base_name: Some("myapp".to_string()),
        shell_parameter_format: Some("cobra".to_string()),
        shells: Some(vec![
            "bash".to_string(),
            "zsh".to_string(),
            "fish".to_string(),
            "pwsh".to_string(),
        ]),
    };
    let rendered = render_generate_completions(&g).unwrap();
    assert_eq!(
        rendered,
        "generate_completions_from_executable \"bin/myapp\", \"completions\",\n    \
             base_name: \"myapp\",\n    \
             shell_parameter_format: :cobra,\n    \
             shells: [:bash, :zsh, :fish, :pwsh]"
    );
}

#[test]
fn test_render_generate_completions_minimal() {
    let g = HomebrewCaskGeneratedCompletions {
        executable: Some("bin/myapp".to_string()),
        args: None,
        base_name: None,
        shell_parameter_format: None,
        shells: None,
    };
    let rendered = render_generate_completions(&g).unwrap();
    assert_eq!(
        rendered,
        "generate_completions_from_executable \"bin/myapp\""
    );
}

#[test]
fn test_render_generate_completions_executable_only_with_format() {
    // Mirrors upstream `generate_completions_default_executable.rb.golden`.
    let g = HomebrewCaskGeneratedCompletions {
        executable: Some("myapp".to_string()),
        args: None,
        base_name: None,
        shell_parameter_format: Some("cobra".to_string()),
        shells: None,
    };
    let rendered = render_generate_completions(&g).unwrap();
    assert_eq!(
        rendered,
        "generate_completions_from_executable \"myapp\",\n    shell_parameter_format: :cobra"
    );
}

#[test]
fn test_render_generate_completions_unknown_format_quotes_string() {
    // Unknown formats fall back to a quoted string (mirrors upstream
    // knownShellParameterFormats fallthrough).
    let g = HomebrewCaskGeneratedCompletions {
        executable: Some("bin/myapp".to_string()),
        args: None,
        base_name: None,
        shell_parameter_format: Some("custom-fmt".to_string()),
        shells: None,
    };
    let rendered = render_generate_completions(&g).unwrap();
    assert!(
        rendered.contains("shell_parameter_format: \"custom-fmt\""),
        "unknown format should be a quoted string\n{rendered}"
    );
}

#[test]
fn test_render_generate_completions_empty_executable_returns_none() {
    let g = HomebrewCaskGeneratedCompletions::default();
    assert!(render_generate_completions(&g).is_none());
}

/// Q1.2 — the `generate_completions_from_executable` directive must render
/// AFTER the `postflight` stanza.
#[test]
fn test_cask_generate_completions_renders_after_postflight() {
    let mut params = empty_cask_params("test", "0.1.0");
    params.postflight = Some("system_command \"chmod\", args: [\"+x\", \"#{bin}/test\"]");
    params.generate_completions = Some(
        "generate_completions_from_executable \"bin/myapp\",\n    shell_parameter_format: :cobra"
            .to_string(),
    );
    let cask = generate_cask(&params).unwrap();
    let post_idx = cask
        .find("postflight do")
        .expect("postflight stanza missing");
    let comp_idx = cask
        .find("generate_completions_from_executable")
        .expect("generate_completions missing");
    assert!(
        comp_idx > post_idx,
        "generate_completions must render after postflight\n{cask}"
    );
}

// ---------------------------------------------------------------------------
// C6 — cask `zap` stanza emits per-key arrays (not hard-coded `trash:`).
// ---------------------------------------------------------------------------

use anodizer_core::config::HomebrewCaskUninstall;

#[test]
fn test_cask_zap_block_emits_each_directive_as_separate_key() {
    let zap_cfg = HomebrewCaskUninstall {
        launchctl: Some(vec!["com.example.daemon".to_string()]),
        quit: Some(vec!["com.example.app".to_string()]),
        login_item: Some(vec!["MyApp".to_string()]),
        delete: Some(vec!["/tmp/foo".to_string()]),
        trash: Some(vec!["~/Library/MyApp".to_string()]),
    };
    let block = super::cask::render_zap_block(Some(&zap_cfg));
    // Each sub-key gets its own Ruby array — the prior code wedged every
    // directive into `zap trash: [...]` as a quoted string, producing
    // syntactically broken Ruby.
    assert!(
        block.contains("zap launchctl: ["),
        "missing launchctl key\n{block}"
    );
    assert!(block.contains("\"com.example.daemon\""));
    assert!(block.contains("quit: ["), "missing quit key\n{block}");
    assert!(block.contains("\"com.example.app\""));
    assert!(
        block.contains("login_item: ["),
        "missing login_item key\n{block}"
    );
    assert!(block.contains("delete: ["), "missing delete key\n{block}");
    assert!(block.contains("trash: ["), "missing trash key\n{block}");
    assert!(block.contains("\"~/Library/MyApp\""));
    // The prior bug wrote `"launchctl: \"...\""` (a quoted string inside a
    // `trash:` array). Make sure we never emit that shape again.
    assert!(
        !block.contains("\"launchctl:"),
        "regression: launchctl directive must not be a quoted string inside trash:\n{block}"
    );
}

#[test]
fn test_cask_zap_block_only_trash() {
    let zap_cfg = HomebrewCaskUninstall {
        trash: Some(vec!["~/Library/Foo".to_string()]),
        ..Default::default()
    };
    let block = super::cask::render_zap_block(Some(&zap_cfg));
    assert!(
        block.starts_with("zap trash: ["),
        "block should start with `zap trash:` when only trash is set\n{block}"
    );
    assert!(block.contains("\"~/Library/Foo\""));
}

#[test]
fn test_cask_zap_block_empty_returns_empty_string() {
    assert_eq!(super::cask::render_zap_block(None), "");
    assert_eq!(
        super::cask::render_zap_block(Some(&HomebrewCaskUninstall::default())),
        ""
    );
}

#[test]
fn test_cask_uninstall_block_uses_array_per_key() {
    let u_cfg = HomebrewCaskUninstall {
        launchctl: Some(vec!["com.example.daemon".to_string()]),
        quit: Some(vec!["com.example.app".to_string()]),
        ..Default::default()
    };
    let block = super::cask::render_uninstall_block(Some(&u_cfg));
    // Canonical shape: `uninstall launchctl: [...], quit: [...]` with
    // arrays — not `uninstall launchctl: "name", quit: "name"`.
    assert!(block.starts_with("uninstall launchctl: ["));
    assert!(block.contains("quit: ["));
    assert!(block.contains("\"com.example.daemon\""));
    assert!(block.contains("\"com.example.app\""));
}

#[test]
fn test_cask_template_renders_multi_key_zap() {
    let mut params = empty_cask_params("test", "0.1.0");
    let zap_block = super::cask::render_zap_block(Some(&HomebrewCaskUninstall {
        launchctl: Some(vec!["com.example.daemon".to_string()]),
        trash: Some(vec!["~/Library/MyApp".to_string()]),
        ..Default::default()
    }));
    params.zap_block = &zap_block;
    let cask = generate_cask(&params).unwrap();
    // Both keys should appear (multi-key emission, not folded into trash).
    assert!(
        cask.contains("zap launchctl: ["),
        "zap launchctl key missing\n{cask}"
    );
    assert!(cask.contains("trash: ["), "trash key missing\n{cask}");
}

// ---------------------------------------------------------------------------
// M4 — cask `additional_url_params` (verified, using, cookies, referer,
//      headers, user_agent, data) renders on the `url` line.
// ---------------------------------------------------------------------------

use anodizer_core::config::HomebrewCaskURL;
use std::collections::HashMap;

#[test]
fn test_render_additional_url_params_full() {
    let mut cookies = HashMap::new();
    cookies.insert("session".to_string(), "deadbeef".to_string());
    let mut data = HashMap::new();
    data.insert("user".to_string(), "alice".to_string());
    let url_cfg = HomebrewCaskURL {
        template: Some("https://example.com/x.zip".to_string()),
        verified: Some("example.com/".to_string()),
        using: Some(":homebrew_curl".to_string()),
        cookies: Some(cookies),
        referer: Some("https://example.com/".to_string()),
        headers: Some(vec!["X-Auth: Bearer xyz".to_string()]),
        user_agent: Some("Mozilla/5.0".to_string()),
        data: Some(data),
    };
    let extras = super::cask::render_additional_url_params(&url_cfg, "      ");
    // Splices directly after the closing `"` of `url "..."`.
    assert!(
        extras.starts_with(",\n      verified: \"example.com/\""),
        "extras must start with `,\\n      verified:` — got:\n{extras}"
    );
    assert!(extras.contains("using: :homebrew_curl"));
    assert!(extras.contains("cookies: {"));
    assert!(extras.contains("\"session\" => \"deadbeef\","));
    assert!(extras.contains("referer: \"https://example.com/\""));
    assert!(extras.contains("header: ["));
    assert!(extras.contains("\"X-Auth: Bearer xyz\""));
    assert!(extras.contains("user_agent: \"Mozilla/5.0\""));
    assert!(extras.contains("data: {"));
    assert!(extras.contains("\"user\" => \"alice\","));
}

#[test]
fn test_render_additional_url_params_empty_returns_empty() {
    let url_cfg = HomebrewCaskURL::default();
    assert_eq!(
        super::cask::render_additional_url_params(&url_cfg, "      "),
        ""
    );
}

#[test]
fn test_render_additional_url_params_verified_only() {
    let url_cfg = HomebrewCaskURL {
        verified: Some("example.com/".to_string()),
        ..Default::default()
    };
    let extras = super::cask::render_additional_url_params(&url_cfg, "      ");
    assert_eq!(extras, ",\n      verified: \"example.com/\"");
}

#[test]
fn test_cask_template_emits_url_extras() {
    let url_cfg = HomebrewCaskURL {
        verified: Some("github.com/org/repo/".to_string()),
        using: Some(":homebrew_curl".to_string()),
        ..Default::default()
    };
    let extras = super::cask::render_additional_url_params(&url_cfg, "      ");
    let mut params = empty_cask_params("test", "0.1.0");
    params.url_extras = &extras;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains("verified: \"github.com/org/repo/\""),
        "verified kwarg missing\n{cask}"
    );
    assert!(
        cask.contains("using: :homebrew_curl"),
        "using kwarg missing\n{cask}"
    );
    // The `url` line must end with `,` (start of the kwargs continuation),
    // not `"`. Validate the splice is correctly attached.
    let url_line = cask
        .lines()
        .find(|l| l.trim_start().starts_with("url \""))
        .expect("url line missing");
    assert!(
        url_line.trim_end().ends_with(",") || url_line.contains("\","),
        "url line should end with `,` to splice into kwargs\nline: {url_line}\n{cask}"
    );
}

// -----------------------------------------------------------------------
// C7 — cask `binary "<n>", target: "<t>"` rename form
// -----------------------------------------------------------------------

/// Bare-string YAML form (`binaries: [my-cli]`) deserialises to the bare
/// `HomebrewCaskBinary::Name` variant and the template emits
/// `binary "my-cli"` — i.e. **no** `target:` kwarg.
#[test]
fn test_cask_binary_bare_string_form_round_trip() {
    use anodizer_core::config::{HomebrewCaskBinary, HomebrewCaskConfig};
    let yaml = r#"
binaries:
  - my-cli
"#;
    let cfg: HomebrewCaskConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let bins = cfg.binaries.expect("binaries should deserialise");
    assert_eq!(bins.len(), 1);
    match &bins[0] {
        HomebrewCaskBinary::Name(n) => assert_eq!(n, "my-cli"),
        other => panic!("expected bare Name variant, got {:?}", other),
    }
    assert_eq!(bins[0].name(), "my-cli");
    assert_eq!(bins[0].target(), None);

    // Render through the template via the same translation the per-crate
    // path performs, then assert the template output.
    let entries = vec![super::cask::CaskBinaryEntry {
        name: bins[0].name().to_string(),
        target: bins[0].target().map(str::to_string),
    }];
    let mut params = empty_cask_params("test", "0.1.0");
    params.binaries = &entries;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains("binary \"my-cli\"\n"),
        "expected bare `binary \"my-cli\"` line\n{cask}"
    );
    // Bare form must NOT emit a `target:` kwarg.
    assert!(
        !cask.contains("binary \"my-cli\", target:"),
        "bare form must not include target:\n{cask}"
    );
}

/// Structured `{ name, target }` YAML form deserialises to the
/// `HomebrewCaskBinary::WithTarget` variant and the template emits
/// `binary "<n>", target: "<t>"`.
#[test]
fn test_cask_binary_object_with_target_renders_target_kwarg() {
    use anodizer_core::config::{HomebrewCaskBinary, HomebrewCaskConfig};
    let yaml = r#"
binaries:
  - name: my-cli
    target: mycli
"#;
    let cfg: HomebrewCaskConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let bins = cfg.binaries.expect("binaries should deserialise");
    assert_eq!(bins.len(), 1);
    match &bins[0] {
        HomebrewCaskBinary::WithTarget { name, target } => {
            assert_eq!(name, "my-cli");
            assert_eq!(target.as_deref(), Some("mycli"));
        }
        other => panic!("expected WithTarget variant, got {:?}", other),
    }
    assert_eq!(bins[0].name(), "my-cli");
    assert_eq!(bins[0].target(), Some("mycli"));

    let entries = vec![super::cask::CaskBinaryEntry {
        name: bins[0].name().to_string(),
        target: bins[0].target().map(str::to_string),
    }];
    let mut params = empty_cask_params("test", "0.1.0");
    params.binaries = &entries;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains("binary \"my-cli\", target: \"mycli\"\n"),
        "expected `binary \"my-cli\", target: \"mycli\"` line\n{cask}"
    );
}

/// Object form WITHOUT `target` set behaves as the bare string form
/// (no `target:` kwarg in the rendered Ruby).
#[test]
fn test_cask_binary_object_without_target_renders_bare() {
    use anodizer_core::config::HomebrewCaskConfig;
    let yaml = r#"
binaries:
  - name: my-cli
"#;
    let cfg: HomebrewCaskConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let bins = cfg.binaries.expect("binaries should deserialise");
    assert_eq!(bins[0].name(), "my-cli");
    assert_eq!(bins[0].target(), None);

    let entries = vec![super::cask::CaskBinaryEntry {
        name: bins[0].name().to_string(),
        target: bins[0].target().map(str::to_string),
    }];
    let mut params = empty_cask_params("test", "0.1.0");
    params.binaries = &entries;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains("binary \"my-cli\"\n"),
        "expected bare `binary \"my-cli\"` line\n{cask}"
    );
    assert!(
        !cask.contains("target:"),
        "object-without-target must not render `target:`\n{cask}"
    );
}

/// Mixed list — bare and object forms in the same `binaries:` array.
#[test]
fn test_cask_binary_mixed_bare_and_target_forms() {
    use anodizer_core::config::HomebrewCaskConfig;
    let yaml = r#"
binaries:
  - bare-tool
  - name: wrapper
    target: actual-bin
"#;
    let cfg: HomebrewCaskConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let bins = cfg.binaries.expect("binaries should deserialise");
    assert_eq!(bins.len(), 2);

    let entries: Vec<super::cask::CaskBinaryEntry> = bins
        .iter()
        .map(|b| super::cask::CaskBinaryEntry {
            name: b.name().to_string(),
            target: b.target().map(str::to_string),
        })
        .collect();
    let mut params = empty_cask_params("test", "0.1.0");
    params.binaries = &entries;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains("binary \"bare-tool\"\n"),
        "missing bare line\n{cask}"
    );
    assert!(
        cask.contains("binary \"wrapper\", target: \"actual-bin\"\n"),
        "missing target-renamed line\n{cask}"
    );
}

// ---------------------------------------------------------------------------
// Multi-archive disambiguation tests
// ---------------------------------------------------------------------------

use anodizer_core::log::{StageLogger, Verbosity};

fn test_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Normal)
}

#[test]
fn test_disambiguate_homebrew_archives_single_per_platform_unchanged() {
    // One archive per platform — output preserves the input tuples byte-for-byte
    // (modulo grouping order which is BTreeMap-sorted by key).
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.gz".to_string(),
            "sha_arm64".to_string(),
            "tar.gz".to_string(),
        ),
        (
            "x86_64-unknown-linux-gnu".to_string(),
            "https://example.com/tool-linux-amd64.tar.gz".to_string(),
            "sha_linux".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let result = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    assert_eq!(result.len(), 2);
    // Verify each input tuple is preserved verbatim in the output.
    let darwin = result
        .iter()
        .find(|(t, _, _)| t == "aarch64-apple-darwin")
        .expect("darwin missing");
    assert_eq!(darwin.1, "https://example.com/tool-darwin-arm64.tar.gz");
    assert_eq!(darwin.2, "sha_arm64");
    let linux = result
        .iter()
        .find(|(t, _, _)| t == "x86_64-unknown-linux-gnu")
        .expect("linux missing");
    assert_eq!(linux.1, "https://example.com/tool-linux-amd64.tar.gz");
    assert_eq!(linux.2, "sha_linux");
}

#[test]
fn test_disambiguate_homebrew_archives_deterministic_order() {
    // Same input run twice must produce byte-identical output (no HashMap
    // randomization). Three platforms, scrambled input order.
    let entries = || {
        vec![
            (
                "x86_64-unknown-linux-gnu".to_string(),
                "https://example.com/linux-amd64.tar.gz".to_string(),
                "sha_linux".to_string(),
                "tar.gz".to_string(),
            ),
            (
                "aarch64-apple-darwin".to_string(),
                "https://example.com/darwin-arm64.tar.gz".to_string(),
                "sha_darwin".to_string(),
                "tar.gz".to_string(),
            ),
            (
                "x86_64-apple-darwin".to_string(),
                "https://example.com/darwin-amd64.tar.gz".to_string(),
                "sha_darwin_intel".to_string(),
                "tar.gz".to_string(),
            ),
        ]
    };
    let r1 = super::publish_formula::disambiguate_homebrew_archives(
        entries(),
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    let r2 = super::publish_formula::disambiguate_homebrew_archives(
        entries(),
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    assert_eq!(r1, r2, "disambiguation output must be deterministic");
}

#[test]
fn test_disambiguate_homebrew_archives_prefers_tar_gz_when_ids_unset() {
    // darwin_arm64 appears twice: once as tar.gz, once as tar.xz.
    // With ids_was_set=false, the tar.gz one must be selected.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.xz".to_string(),
            "sha_xz".to_string(),
            "tar.xz".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.gz".to_string(),
            "sha_gz".to_string(),
            "tar.gz".to_string(),
        ),
        (
            "x86_64-unknown-linux-gnu".to_string(),
            "https://example.com/tool-linux-amd64.tar.gz".to_string(),
            "sha_linux".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let result = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    // Should have exactly two entries (one per platform).
    assert_eq!(result.len(), 2);
    // The darwin_arm64 entry must be the tar.gz one.
    let darwin = result
        .iter()
        .find(|(t, _, _)| t.contains("apple-darwin"))
        .expect("darwin entry missing");
    assert_eq!(darwin.2, "sha_gz", "expected tar.gz sha, got {}", darwin.2);
    assert!(
        darwin.1.ends_with(".tar.gz"),
        "expected tar.gz url, got {}",
        darwin.1
    );
}

#[test]
fn test_disambiguate_homebrew_archives_tgz_alias_accepted() {
    // Format "tgz" (alternative alias) must also be preferred.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.zst".to_string(),
            "sha_zst".to_string(),
            "tar.zst".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tgz".to_string(),
            "sha_tgz".to_string(),
            "tgz".to_string(),
        ),
    ];
    let result = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].2, "sha_tgz");
}

#[test]
fn test_disambiguate_homebrew_archives_errors_when_ids_set_and_duplicate() {
    // With ids_was_set=true any remaining duplicate is a config error.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64-a.tar.gz".to_string(),
            "sha_a".to_string(),
            "tar.gz".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64-b.tar.gz".to_string(),
            "sha_b".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let err = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        true,
        "anodizer",
        &test_log(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("homebrew:"), "missing prefix: {msg}");
    assert!(
        msg.contains("crate 'anodizer'"),
        "missing crate name: {msg}"
    );
    assert!(
        msg.contains("multiple archives found for"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("tool-darwin-arm64-a.tar.gz") && msg.contains("tool-darwin-arm64-b.tar.gz"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_homebrew_archives_errors_when_no_tar_gz_and_ambiguous() {
    // Two non-tar.gz archives for the same platform, ids unset → error.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.xz".to_string(),
            "sha_xz".to_string(),
            "tar.xz".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.zst".to_string(),
            "sha_zst".to_string(),
            "tar.zst".to_string(),
        ),
    ];
    let err = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("homebrew:"), "missing prefix: {msg}");
    assert!(
        msg.contains("crate 'anodizer'"),
        "missing crate name: {msg}"
    );
    assert!(
        msg.contains("none matches a preferred format"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("tool-darwin-arm64.tar.xz") && msg.contains("tool-darwin-arm64.tar.zst"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_homebrew_archives_errors_when_multiple_tar_gz_unset_ids() {
    // Two tar.gz archives for the same platform with ids unset — the
    // preferred-format bucket itself is ambiguous, so we must still error.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-A-darwin-arm64.tar.gz".to_string(),
            "sha_a".to_string(),
            "tar.gz".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-B-darwin-arm64.tar.gz".to_string(),
            "sha_b".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let err = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("homebrew:"), "missing prefix: {msg}");
    assert!(
        msg.contains("multiple .tar.gz archives"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("tool-A-darwin-arm64.tar.gz") && msg.contains("tool-B-darwin-arm64.tar.gz"),
        "error must name conflicting artifacts: {msg}"
    );
}

#[test]
fn test_disambiguate_homebrew_archives_ids_set_no_duplicates_passes() {
    // ids_was_set=true with one archive per platform — pass-through OK.
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.gz".to_string(),
            "sha_arm64".to_string(),
            "tar.gz".to_string(),
        ),
        (
            "x86_64-unknown-linux-gnu".to_string(),
            "https://example.com/tool-linux-amd64.tar.gz".to_string(),
            "sha_linux".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let result = super::publish_formula::disambiguate_homebrew_archives(
        entries,
        true,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    assert_eq!(result.len(), 2);
}

#[test]
fn test_disambiguate_homebrew_archives_empty_input() {
    // Empty input yields empty output, never an error.
    let result = super::publish_formula::disambiguate_homebrew_archives(
        vec![],
        false,
        "anodizer",
        &test_log(),
    )
    .unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_disambiguate_homebrew_archives_logs_dropped_via_sink() {
    // Two archives for the same (os, arch) with ids unset: the fallback
    // keeps the .tar.gz and drops the .tar.xz. Capture the warn sink to
    // assert both URLs appear in the emitted log line (proves the user
    // sees what was discarded — not silent disambiguation).
    let entries = vec![
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.xz".to_string(),
            "sha_xz".to_string(),
            "tar.xz".to_string(),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            "https://example.com/tool-darwin-arm64.tar.gz".to_string(),
            "sha_gz".to_string(),
            "tar.gz".to_string(),
        ),
    ];
    let mut captured: Vec<String> = Vec::new();
    let result = crate::util::disambiguate_by_format_with_sink(
        entries,
        |(target, _, _, _)| {
            let (os, arch) = anodizer_core::target::map_target(target);
            format!("{os}_{arch}")
        },
        |(_, _, _, fmt)| fmt.as_str(),
        |(_, url, _, _)| url.clone(),
        crate::util::DisambiguateInnerConfig {
            preferred_formats: super::publish_formula::HOMEBREW_PREFERRED_FORMATS,
            ids_was_set: false,
            publisher_label: "homebrew",
            crate_name: "anodizer",
        },
        &mut |msg| captured.push(msg.to_string()),
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(captured.len(), 1, "expected exactly one warn line");
    let line = &captured[0];
    assert!(
        line.starts_with("homebrew:"),
        "warn line should carry publisher prefix: {line}"
    );
    assert!(
        line.contains("crate 'anodizer'"),
        "warn line should name the crate: {line}"
    );
    assert!(
        line.contains("tool-darwin-arm64.tar.gz") && line.contains("(.tar.gz)"),
        "warn line should name the kept archive: {line}"
    );
    assert!(
        line.contains("tool-darwin-arm64.tar.xz") && line.contains("(.tar.xz)"),
        "warn line should name the dropped archive: {line}"
    );
}

// ===========================================================================
// publish_to_homebrew / publish_cask / publish_top_level_homebrew_casks
// early-exit tests.
//
// These pin the silent-skip contract — each guard that prevents a real push
// MUST return Ok(false) (or Ok(()) for publish_cask), and the upstream
// callers use the boolean to decide whether to record rollback evidence.
// A regression that returned `Ok(true)` would cause the rollback orchestrator
// to attempt `git revert HEAD` in a temp clone that has nothing this run
// actually changed.
// ===========================================================================

use anodizer_core::config::{
    Config, CrateConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig, RepositoryConfig,
    StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};

fn quiet_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

fn hb_ctx(hb_cfg: HomebrewConfig, dry_run: bool) -> Context {
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                homebrew: Some(hb_cfg),
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

/// `resolve_cask_directory`: an unset directory falls back to "Casks" and a
/// plain (non-template) value renders verbatim.
#[test]
fn resolve_cask_directory_defaults_and_renders() {
    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert_eq!(super::resolve_cask_directory(None, &ctx).unwrap(), "Casks");
    assert_eq!(
        super::resolve_cask_directory(Some("Casks/versioned"), &ctx).unwrap(),
        "Casks/versioned"
    );
}

/// `resolve_cask_directory`: an invalid `directory` template PROPAGATES the
/// render error instead of swallowing it into a literal-braces path that would
/// be committed + pushed to the tap.
#[test]
fn resolve_cask_directory_invalid_template_errors() {
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let result = super::resolve_cask_directory(Some("Casks/{{ unclosed"), &ctx);
    let err =
        result.expect_err("invalid directory template must error, not yield a literal-braces path");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("render `directory` template"),
        "error must name the directory render failure; got: {msg}"
    );
}

/// publish_to_homebrew: missing `publish.homebrew` block => actionable error.
#[test]
fn publish_to_homebrew_missing_config_errors() {
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no homebrew config"), "got: {msg}");
    assert!(msg.contains("mytool"));
}

/// publish_to_homebrew: `skip_upload: true` returns Ok(false) before any work.
#[test]
fn publish_to_homebrew_skip_upload_true_returns_false() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        skip_upload: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let got = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap();
    assert!(!got, "skip_upload=true must short-circuit Ok(false)");
}

/// publish_to_homebrew: missing repository => actionable error citing crate.
#[test]
fn publish_to_homebrew_missing_repository_errors() {
    let cfg = HomebrewConfig {
        repository: None,
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let err = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no repository config"), "{msg}");
    assert!(msg.contains("mytool"));
}

/// publish_to_homebrew: dry-run returns Ok(false) (no push).
#[test]
fn publish_to_homebrew_dry_run_returns_false() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, true);
    let got = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap();
    assert!(!got, "dry-run must return Ok(false)");
}

/// publish_to_homebrew: no archive artifacts => bail with actionable error
/// mentioning the filter knobs (ids / amd64_variant / arm_variant). With
/// neither variant configured, both hints carry the `<default …>` marker so
/// the operator can tell a fallback apart from an explicit setting.
/// Prevents a broken formula with empty url/sha256 from being written.
#[test]
fn publish_to_homebrew_no_archives_errors() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let err = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no archives matched filters"), "{msg}");
    assert!(msg.contains("mytool"));
    assert!(msg.contains("amd64_variant=<default v1>"), "{msg}");
    assert!(msg.contains("arm_variant=<default 6>"), "{msg}");
}

/// publish_to_homebrew: when the variant selectors ARE configured, the
/// no-archives error prints the configured values plainly — no `<default …>`
/// marker that would misattribute an operator choice to a fallback.
#[test]
fn publish_to_homebrew_no_archives_errors_configured_variants() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        amd64_variant: Some(anodizer_core::config::Amd64Variant::V3),
        arm_variant: Some("7".to_string()),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let err = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no archives matched filters"), "{msg}");
    assert!(msg.contains("amd64_variant=v3,"), "{msg}");
    assert!(msg.contains("arm_variant=7)"), "{msg}");
    assert!(!msg.contains("<default"), "{msg}");
}

/// publish_cask: missing `publish.homebrew` => error.
#[test]
fn publish_cask_missing_homebrew_config_errors() {
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no homebrew config"), "{msg}");
}

/// publish_cask: homebrew set but `cask:` block absent => error.
#[test]
fn publish_cask_missing_cask_config_errors() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: None,
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no cask config"), "{msg}");
}

/// publish_cask: cask-level `skip_upload: true` takes precedence over the
/// formula's skip_upload and skips before any git work.
#[test]
fn publish_cask_cask_skip_upload_returns_ok() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig {
            skip_upload: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    super::publish_cask(&mut ctx, "mytool", &quiet_log())
        .expect("skip_upload=true must succeed without pushing");
}

/// publish_cask: missing repository => error citing crate name.
#[test]
fn publish_cask_missing_repository_errors() {
    let cfg = HomebrewConfig {
        repository: None,
        cask: Some(HomebrewCaskConfig::default()),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no repository config"), "{msg}");
    assert!(msg.contains("mytool"));
}

/// publish_cask: dry-run short-circuits to Ok(()).
#[test]
fn publish_cask_dry_run_returns_ok() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig {
            name: Some("mytool".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, true);
    super::publish_cask(&mut ctx, "mytool", &quiet_log()).expect("dry-run must succeed");
}

/// publish_top_level_homebrew_casks: empty list returns Ok(false).
#[test]
fn publish_top_level_homebrew_casks_empty_returns_false() {
    let config = Config {
        homebrew_casks: None,
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap();
    assert!(!got.pushed_any, "no entries => pushed_any false");
    assert_eq!(got.total, 0);
    assert_eq!(got.applicable, 0);
}

/// publish_top_level_homebrew_casks: list present but empty vec returns
/// `TopLevelCaskRunResult::default()`.
#[test]
fn publish_top_level_homebrew_casks_empty_vec_returns_false() {
    let config = Config {
        homebrew_casks: Some(vec![]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap();
    assert!(!got.pushed_any);
    assert_eq!(got.total, 0);
}

/// publish_top_level_homebrew_casks: missing repository on an entry => error
/// citing the cask name (operators need to know which entry is mis-configured).
#[test]
fn publish_top_level_homebrew_casks_missing_repository_errors() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: None,
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no repository config"), "{msg}");
    assert!(msg.contains("mycask"));
}

/// publish_top_level_homebrew_casks: dry-run returns Ok(false) for every
/// entry (no actual push).
#[test]
fn publish_top_level_homebrew_casks_dry_run_returns_false() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap();
    assert!(!got.pushed_any, "dry-run must not push");
    assert_eq!(got.total, 1);
}

/// publish_top_level_homebrew_casks: `skip_upload: true` on the entry
/// short-circuits to a continue (no push, no error) and the function
/// reports `pushed_any: false` when every entry skipped.
#[test]
fn publish_top_level_homebrew_casks_skip_upload_returns_false() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            skip_upload: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap();
    assert!(!got.pushed_any, "every entry skipped => pushed_any false");
    assert_eq!(got.total, 1);
}

/// publish_top_level_homebrew_casks: an `ids:` filter that matches no macOS
/// artifact WHILE other macOS artifacts exist is a typo signal — the publisher
/// errors instead of silently skipping (which would let `brew install` 404).
#[test]
fn publish_top_level_homebrew_casks_ids_typo_errors() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            ids: Some(vec!["nighty".to_string()]), // typo for "nightly"
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // A darwin artifact exists, but under a DIFFERENT id ("stable").
    let mut a = art_with_url_sha(
        ArtifactKind::DiskImage,
        "mytool.dmg",
        "aarch64-apple-darwin",
        "https://e.com/mytool.dmg",
        "dmgsha",
    );
    a.metadata.insert("id".to_string(), "stable".to_string());
    ctx.artifacts.add(a);

    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("mycask"), "{msg}");
    assert!(
        msg.contains("ids"),
        "error must call out the ids filter: {msg}"
    );
}

/// Converse of the typo case: when there is genuinely NO macOS artifact at
/// all, an `ids:` filter that matches nothing is NOT a typo signal — the
/// publisher skips (NotApplicable), it does not error.
#[test]
fn publish_top_level_homebrew_casks_no_macos_with_ids_skips_not_errors() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            ids: Some(vec!["stable".to_string()]),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // Only a Linux artifact — no darwin build exists at all.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool.tar.gz",
        "linuxsha",
    ));

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("no darwin build at all must skip, not error");
    assert!(!got.pushed_any);
    assert_eq!(got.applicable, 0, "no applicable cask when no darwin build");
}

// ===========================================================================
// generate_cask_from_context — exercise the multi-platform / fallback paths
// in cask.rs that aren't reachable through the bare `generate_cask` API.
// ===========================================================================

use anodizer_core::artifact::{Artifact, ArtifactKind};

fn art_with_url_sha(
    kind: ArtifactKind,
    name: &str,
    target: &str,
    url: &str,
    sha: &str,
) -> Artifact {
    let mut metadata = HashMap::new();
    metadata.insert("url".to_string(), url.to_string());
    metadata.insert("sha256".to_string(), sha.to_string());
    Artifact {
        kind,
        path: std::path::PathBuf::from(format!("/tmp/{name}")),
        name: name.to_string(),
        target: Some(target.to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    }
}

/// `find_top_level_cask_artifact` prefers DiskImage over Archive when both
/// are available for darwin. Pins the selection order.
#[test]
fn find_top_level_cask_artifact_prefers_disk_image_over_archive() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool.tar.gz",
        "archsha",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::DiskImage,
        "mytool.dmg",
        "aarch64-apple-darwin",
        "https://e.com/mytool.dmg",
        "dmgsha",
    ));
    let got = super::cask_scope::find_top_level_cask_artifact(&ctx, None).expect("artifact found");
    assert_eq!(got.kind, ArtifactKind::DiskImage, "DiskImage preferred");
}

/// `find_top_level_cask_artifact` falls back to Archive when no DiskImage
/// is present.
#[test]
fn find_top_level_cask_artifact_falls_back_to_archive() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool.tar.gz",
        "archsha",
    ));
    let got = super::cask_scope::find_top_level_cask_artifact(&ctx, None).expect("artifact found");
    assert_eq!(got.kind, ArtifactKind::Archive);
}

/// `find_top_level_cask_artifact` returns None when nothing matches darwin/macos.
#[test]
fn find_top_level_cask_artifact_returns_none_for_no_macos() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool.tar.gz",
        "linuxsha",
    ));
    assert!(super::cask_scope::find_top_level_cask_artifact(&ctx, None).is_none());
}

/// `find_top_level_cask_artifact` excludes Apple-but-not-macOS archives
/// (`*-apple-ios`/`-watchos`/`-tvos`): they match the old broad
/// `contains("apple")` selector but carry no `brew`-installable binary.
#[test]
fn find_top_level_cask_artifact_excludes_apple_non_macos() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    for triple in [
        "aarch64-apple-ios",
        "aarch64-apple-watchos",
        "aarch64-apple-tvos",
    ] {
        ctx.artifacts.add(art_with_url_sha(
            ArtifactKind::Archive,
            &format!("mytool-{triple}.tar.gz"),
            triple,
            "https://e.com/mytool.tar.gz",
            "sha",
        ));
    }
    assert!(
        super::cask_scope::find_top_level_cask_artifact(&ctx, None).is_none(),
        "iOS/watchOS/tvOS archives must not be selected as a cask url"
    );
}

/// `find_top_level_cask_artifact` with an IDs filter excludes non-matching
/// artifacts. Pins the ids filter behaviour.
#[test]
fn find_top_level_cask_artifact_filters_by_id() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut a = art_with_url_sha(
        ArtifactKind::DiskImage,
        "mytool.dmg",
        "aarch64-apple-darwin",
        "https://e.com/mytool.dmg",
        "dmgsha",
    );
    a.metadata.insert("id".to_string(), "stable".to_string());
    ctx.artifacts.add(a);
    // Only `nightly` IDs are wanted => returns None.
    assert!(
        super::cask_scope::find_top_level_cask_artifact(&ctx, Some(&["nightly".to_string()]))
            .is_none()
    );
    // `stable` IDs requested => the artifact is returned.
    let got = super::cask_scope::find_top_level_cask_artifact(&ctx, Some(&["stable".to_string()]))
        .expect("artifact must match");
    assert_eq!(got.kind, ArtifactKind::DiskImage);
}

/// `generate_cask` with a multi-platform `platforms` payload uses the
/// per-arch on_intel / on_arm blocks INSIDE on_macos / on_linux, with
/// the top-level url/sha256 elided.
#[test]
fn generate_cask_multi_platform_emits_per_arch_blocks_without_top_level_url() {
    use super::cask::{CaskArchEntry, CaskParams, CaskPlatformBlock};
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "ignored-when-platforms-present",
        url: "https://ignored.example/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: vec![
            CaskPlatformBlock {
                os_block: "macos".to_string(),
                arches: vec![
                    CaskArchEntry {
                        arch_block: "intel".to_string(),
                        url: "https://example.com/intel.tar.gz".to_string(),
                        sha256: "intelsha".to_string(),
                    },
                    CaskArchEntry {
                        arch_block: "arm".to_string(),
                        url: "https://example.com/arm.tar.gz".to_string(),
                        sha256: "armsha".to_string(),
                    },
                ],
            },
            CaskPlatformBlock {
                os_block: "linux".to_string(),
                arches: vec![CaskArchEntry {
                    arch_block: "intel".to_string(),
                    url: "https://example.com/linux.tar.gz".to_string(),
                    sha256: "linuxsha".to_string(),
                }],
            },
        ],
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    // on_macos / on_linux scaffolding present.
    assert!(cask.contains("on_macos do"));
    assert!(cask.contains("on_linux do"));
    assert!(cask.contains("on_intel do"));
    assert!(cask.contains("on_arm do"));
    // All three shas appear inside the per-arch blocks.
    assert!(cask.contains("intelsha"));
    assert!(cask.contains("armsha"));
    assert!(cask.contains("linuxsha"));
    // The top-level url/sha256 must NOT be rendered when `has_platforms` is true.
    assert!(
        !cask.contains("ignored-when-platforms-present"),
        "top-level sha256 must be elided when platforms are set"
    );
    assert!(!cask.contains("https://ignored.example/x.tar.gz"));
}

/// Multi-arch regression: a release carrying darwin-arm64 + darwin-amd64
/// AND linux-amd64 + linux-arm64 archives must emit EVERY (os, arch) pair
/// in the cask — both `on_arm`/`on_intel` under `on_macos`, and a matching
/// `on_linux` block. The v0.9.1 cask shipped arm64-only because the
/// per-arch dedup filter compared the *current* artifact's OS against each
/// existing entry, so a macOS entry whose arch matched a later Linux
/// artifact wrongly suppressed the Linux entry (and vice-versa).
#[test]
fn generate_cask_from_context_emits_every_os_arch_pair() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    // All four archives a typical macOS+Linux release produces. Distinct
    // sha256s so a dropped arch is detectable by a missing digest.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.0.0-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/mytool-1.0.0-darwin-amd64.tar.gz",
        "sha_darwin_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux-amd64.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool-1.0.0-linux-amd64.tar.gz",
        "sha_linux_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux-arm64.tar.gz",
        "aarch64-unknown-linux-gnu",
        "https://e.com/mytool-1.0.0-linux-arm64.tar.gz",
        "sha_linux_arm64",
    ));

    let hb_cfg = HomebrewConfig::default();
    let cask_cfg = HomebrewCaskConfig::default();
    let log = test_log();
    let result =
        super::cask_scope::generate_cask_from_context(&ctx, "mytool", &hb_cfg, &cask_cfg, &log)
            .expect("multi-arch cask generation");
    let cask = result.content;

    assert!(cask.contains("on_macos do"), "missing on_macos\n{cask}");
    assert!(cask.contains("on_linux do"), "missing on_linux\n{cask}");
    // Every one of the four digests must survive into the rendered cask.
    for sha in [
        "sha_darwin_arm64",
        "sha_darwin_amd64",
        "sha_linux_amd64",
        "sha_linux_arm64",
    ] {
        assert!(
            cask.contains(sha),
            "cask dropped the {sha} arch entry — multi-arch dedup bug\n{cask}"
        );
    }
    // Both macOS arch stanzas present; Linux block carries both arches too.
    assert_eq!(
        cask.matches("on_arm do").count(),
        2,
        "expected on_arm under both on_macos and on_linux\n{cask}"
    );
    assert_eq!(
        cask.matches("on_intel do").count(),
        2,
        "expected on_intel under both on_macos and on_linux\n{cask}"
    );
}

/// Single-arch regression guard: a darwin-arm64-only release must still
/// produce a valid cask with exactly that one arch entry (no spurious
/// extra stanzas, no failure).
#[test]
fn generate_cask_from_context_single_arch_still_valid() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.0.0-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    let hb_cfg = HomebrewConfig::default();
    let cask_cfg = HomebrewCaskConfig::default();
    let log = test_log();
    let result =
        super::cask_scope::generate_cask_from_context(&ctx, "mytool", &hb_cfg, &cask_cfg, &log)
            .expect("single-arch cask generation");
    let cask = result.content;
    assert!(cask.contains("sha_darwin_arm64"), "{cask}");
    assert!(
        !cask.contains("on_intel do"),
        "single arm64 release must not emit a spurious on_intel stanza\n{cask}"
    );
}

/// `generate_cask` with alternative_names emits multiple `name` lines.
#[test]
fn generate_cask_emits_alternative_names() {
    use super::cask::CaskParams;
    let alts = vec!["alt-1".to_string(), "alt-2".to_string()];
    let params = CaskParams {
        name: "mytool",
        display_name: "MyTool",
        alternative_names: &alts,
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("name \"MyTool\""));
    assert!(cask.contains("name \"alt-1\""));
    assert!(cask.contains("name \"alt-2\""));
}

/// An unset cask `livecheck` keeps today's behaviour: the cask emits
/// `livecheck do\n  skip "Auto-generated on release."\nend` — a binary cask's
/// download URL/sha256 are rewritten every release, so there's nothing stable
/// to poll. This must not regress when the configurable field is absent.
#[test]
fn generate_cask_unset_livecheck_emits_default_skip() {
    let params = empty_cask_params("mytool", "1.0.0");
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(
        cask.contains("livecheck do\n    skip \"Auto-generated on release.\"\n  end"),
        "unset livecheck must default to the skip stanza:\n{cask}"
    );
    assert!(
        !cask.contains("strategy :"),
        "unset livecheck must not emit an active strategy:\n{cask}"
    );
}

/// A configured cask `livecheck` (github_latest strategy) renders a real
/// active `livecheck do … end` block, mirroring the formula renderer. The
/// `url :url` symbol shorthand routes through the shared `render_livecheck`.
#[test]
fn generate_cask_configured_livecheck_emits_active_block() {
    use anodizer_core::config::HomebrewLivecheck;
    let body = super::formula::render_livecheck(
        Some(&HomebrewLivecheck {
            skip: Some(false),
            skip_reason: None,
            strategy: Some("github_latest".to_string()),
            url: Some("url".to_string()),
            regex: None,
        }),
        &test_log(),
    );
    assert_eq!(
        body.as_deref(),
        Some("url :url\nstrategy :github_latest"),
        "active livecheck body shape"
    );
    let mut params = empty_cask_params("mytool", "1.0.0");
    params.livecheck = body;
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(
        cask.contains("livecheck do\n    url :url\n    strategy :github_latest\n  end"),
        "configured livecheck must emit an active block:\n{cask}"
    );
    assert!(
        !cask.contains("skip \"Auto-generated"),
        "active livecheck must not also skip:\n{cask}"
    );
}

/// `skip: false` with no `strategy`/`url`/`regex` is an invalid active
/// livecheck (empty `livecheck do … end`), so the shared renderer falls back
/// to `skip` rather than emitting broken Ruby — same discipline the formula
/// path uses.
#[test]
fn generate_cask_livecheck_skip_false_without_strategy_falls_back_to_skip() {
    use anodizer_core::config::HomebrewLivecheck;
    let body = super::formula::render_livecheck(
        Some(&HomebrewLivecheck {
            skip: Some(false),
            ..Default::default()
        }),
        &test_log(),
    );
    let mut params = empty_cask_params("mytool", "1.0.0");
    params.livecheck = body;
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(
        cask.contains("skip \"Auto-generated on release.\""),
        "skip:false without a strategy must fall back to skip:\n{cask}"
    );
}

/// End-to-end through `generate_cask_from_context`: a `homebrew_casks`-shaped
/// `HomebrewCaskConfig` carrying `livecheck` renders the active block in the
/// final cask file (proves the field is wired from config to output, not just
/// the param-level renderer).
#[test]
fn generate_cask_from_context_renders_configured_livecheck() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig, HomebrewLivecheck};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.0.0-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    let hb_cfg = HomebrewConfig::default();
    let cask_cfg = HomebrewCaskConfig {
        livecheck: Some(HomebrewLivecheck {
            skip: Some(false),
            strategy: Some("github_latest".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = super::cask_scope::generate_cask_from_context(
        &ctx,
        "mytool",
        &hb_cfg,
        &cask_cfg,
        &test_log(),
    )
    .expect("cask generation");
    assert!(
        result
            .content
            .contains("livecheck do\n    url :stable\n    strategy :github_latest\n  end"),
        "configured livecheck must reach the rendered cask file:\n{}",
        result.content
    );
}

/// `generate_cask` with manpages emits a `manpage` line per entry.
#[test]
fn generate_cask_emits_manpages() {
    use super::cask::CaskParams;
    let pages = vec![
        "share/man/man1/x.1".to_string(),
        "share/man/man1/y.1".to_string(),
    ];
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &pages,
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("manpage \"share/man/man1/x.1\""));
    assert!(cask.contains("manpage \"share/man/man1/y.1\""));
}

/// `generate_cask` with completions emits `bash_completion` / `zsh_completion`
/// / `fish_completion` lines only when configured.
#[test]
fn generate_cask_emits_completion_lines_only_for_set_shells() {
    use super::cask::CaskParams;
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: Some("share/bash-completion/mytool"),
        completions_zsh: Some("share/zsh/site-functions/_mytool"),
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("bash_completion \"share/bash-completion/mytool\""));
    assert!(cask.contains("zsh_completion \"share/zsh/site-functions/_mytool\""));
    assert!(
        !cask.contains("fish_completion"),
        "fish completion must NOT render when None"
    );
}

/// `generate_cask` with all hooks renders preflight/postflight/uninstall_preflight/postflight.
#[test]
fn generate_cask_emits_all_hooks() {
    use super::cask::CaskParams;
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: Some("    puts 'pre'"),
        postflight: Some("    puts 'post'"),
        uninstall_preflight: Some("    puts 'unpre'"),
        uninstall_postflight: Some("    puts 'unpost'"),
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("preflight do"));
    assert!(cask.contains("postflight do"));
    assert!(cask.contains("uninstall_preflight do"));
    assert!(cask.contains("uninstall_postflight do"));
    assert!(cask.contains("'pre'"));
    assert!(cask.contains("'post'"));
    assert!(cask.contains("'unpre'"));
    assert!(cask.contains("'unpost'"));
}

/// `generate_cask` with `service` field emits the service block.
#[test]
fn generate_cask_emits_service_block() {
    use super::cask::CaskParams;
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: Some("    run [opt_bin/\"mytool\", \"--daemon\"]"),
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("service do"));
    assert!(cask.contains("--daemon"));
}

/// `generate_cask` emits `depends_on` and `conflicts_with` directives
/// when set.
#[test]
fn generate_cask_emits_depends_and_conflicts() {
    use super::cask::CaskParams;
    let deps = vec!["cask: \"other-app\"".to_string()];
    let cfs = vec!["cask: \"old-app\"".to_string()];
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &deps,
        conflicts_with: &cfs,
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("depends_on cask: \"other-app\""));
    assert!(cask.contains("conflicts_with cask: \"old-app\""));
}

/// `generate_cask` with `custom_block` injects raw Ruby into the cask.
#[test]
fn generate_cask_emits_custom_block() {
    use super::cask::CaskParams;
    let params = CaskParams {
        name: "mytool",
        display_name: "mytool",
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://e.com/x.tar.gz",
        url_extras: "",
        url_extras_indented: "",
        homepage: None,
        description: None,
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: Some("  auto_updates true"),
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &[],
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("auto_updates true"));
}

/// Building a multi-platform homebrew cask for an artifact whose `sha256`
/// metadata is empty must bail with an actionable error. A multi-platform
/// cask block with `sha256 ""` fails `brew style` and aborts `brew
/// install`. The bail message must name the publisher, the field, and the
/// offending artifact context (os/arch/crate).
#[test]
fn homebrew_cask_sha256_empty_metadata_bails_with_actionable_error() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig::default()),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    // Add two darwin artifacts: one with sha256 (intel) so the find-primary
    // path succeeds, one without (arm) to exercise the multi-platform bail.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/intel.tar.gz",
        "intelsha",
    ));
    let mut arm = art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/arm.tar.gz",
        "armsha",
    );
    arm.metadata.remove("sha256");
    ctx.artifacts.add(arm);
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log())
        .expect_err("missing sha256 in cask must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("homebrew cask:") && msg.contains("sha256"),
        "error must name publisher + field; got: {msg}"
    );
    assert!(
        msg.contains("mytool-darwin-arm64.tar.gz") || msg.contains("mytool"),
        "error must name the offending artifact / crate; got: {msg}"
    );
    assert!(
        msg.contains("dist/artifacts.json") || msg.contains("re-run"),
        "error must include a next-step hint; got: {msg}"
    );
}

/// `publish_top_level_homebrew_casks`: when the macOS artifact has no `url`
/// metadata AND the cask config specifies a `url:` block without
/// `template`, the publisher must bail rather than silently emit an empty
/// `url ""` line (which `brew style` rejects and which fails on `brew
/// install` because there's no download endpoint).
#[test]
fn homebrew_top_level_cask_url_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::config::HomebrewCaskURL;
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            // url block present but `template:` unset → forces the
            // `unwrap_or_default()` path that used to silently emit `url ""`.
            url: Some(HomebrewCaskURL {
                template: None,
                ..Default::default()
            }),
            ..Default::default()
        }]),
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // macOS artifact with `id=primary` so the IDs filter matches it, but
    // WITHOUT `url` metadata.
    let mut metadata = HashMap::new();
    metadata.insert("sha256".to_string(), "dmgsha".to_string());
    metadata.insert("id".to_string(), "primary".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DiskImage,
        path: std::path::PathBuf::from("/tmp/mycask.dmg"),
        name: "mycask.dmg".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    });
    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect_err("missing url metadata must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("homebrew_casks:") && msg.contains("url"),
        "error must name publisher + field; got: {msg}"
    );
    assert!(
        msg.contains("mycask"),
        "error must name the offending cask; got: {msg}"
    );
    assert!(
        msg.contains("url.template") || msg.contains("metadata.url") || msg.contains("re-run"),
        "error must include an actionable next-step hint; got: {msg}"
    );
}

// ===========================================================================
// publish_top_level_homebrew_casks + publish_cask — inner-loop branch
// coverage beyond the early-exit guards: artifact-resolution failures,
// project_name fallback, no-URL-block bail, and the skip_upload fallback
// ladder from cask-level to homebrew-level.
// ===========================================================================

/// publish_top_level_homebrew_casks: a valid entry (repo set, no skip,
/// no dry-run) with NO macOS artifact in the bundle must bail with the
/// cask name in the message — operators need to know which entry is
/// missing its macOS artifact.
#[test]
fn publish_top_level_homebrew_casks_no_macos_artifact_errors_with_cask_name() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // Linux-only artifact; the darwin selector returns None. The cask
    // entry is *inapplicable* to the current scope (no darwin Archive),
    // not a publish failure — the run returns Ok with `applicable: 0`
    // and the HomebrewPublisher caller maps that to
    // `Skipped(NotApplicable)`.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool.tar.gz",
        "linuxsha",
    ));
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("inapplicable cask must skip cleanly, not error");
    assert!(!got.pushed_any);
    assert_eq!(got.total, 1);
    assert_eq!(got.applicable, 0, "no darwin artifact => not applicable");
}

/// publish_top_level_homebrew_casks: the cask name defaults to
/// `config.project_name` when the entry omits `name:`. Without this
/// fallback, an empty-name cask would render `<empty>.rb` in the tap.
#[test]
fn publish_top_level_homebrew_casks_defaults_name_to_project_name() {
    let config = Config {
        project_name: "myproject".to_string(),
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            // name unset on purpose.
            name: None,
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    // No darwin artifact -> skips through the artifact lookup as
    // not-applicable; the project_name fallback surfaces in the
    // status-line log. Captured via the structured result; tests for
    // the log-line wording live in the per-iteration tests above.
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("name fallback path must skip cleanly when no macOS artifact");
    assert!(!got.pushed_any);
    assert_eq!(got.total, 1);
    assert_eq!(got.applicable, 0);
}

/// publish_top_level_homebrew_casks: when no `url:` block is configured AND
/// the macOS artifact lacks `url` metadata, the publisher bails through the
/// alternate `else` arm — distinct from the `url:`-block-with-no-template
/// path already covered above. Error must cite the cask name and the
/// `url.template` hint.
#[test]
fn publish_top_level_homebrew_casks_no_url_block_no_metadata_url_errors() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            // url block absent — different bail arm from the existing
            // `HomebrewCaskURL { template: None, .. }` test.
            url: None,
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut metadata = HashMap::new();
    metadata.insert("sha256".to_string(), "dmgsha".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DiskImage,
        path: std::path::PathBuf::from("/tmp/mycask.dmg"),
        name: "mycask.dmg".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    });
    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("mycask"), "must cite cask name; got: {msg}");
    assert!(
        msg.contains("url.template"),
        "no-url-block bail must hint at url.template; got: {msg}"
    );
}

/// publish_top_level_homebrew_casks: artifact has `url` metadata but no
/// `sha256`. Bail must cite the cask name and the `sha256` field — a cask
/// rendered with an empty `sha256 ""` line fails `brew install`.
#[test]
fn publish_top_level_homebrew_casks_no_sha256_errors_with_cask_name() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-cask-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut metadata = HashMap::new();
    metadata.insert("url".to_string(), "https://e.com/mycask.dmg".to_string());
    // sha256 intentionally absent.
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DiskImage,
        path: std::path::PathBuf::from("/tmp/mycask.dmg"),
        name: "mycask.dmg".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    });
    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("mycask"), "must cite cask name; got: {msg}");
    assert!(
        msg.contains("sha256"),
        "must cite the sha256 field; got: {msg}"
    );
}

/// publish_top_level_homebrew_casks: a list with a `skip_upload:true` entry
/// followed by a missing-repository entry continues past the first and
/// surfaces the second entry's bail — proves the loop iterates past
/// `continue` and that `?` propagation cites the failing entry, not the
/// first-by-index.
#[test]
fn publish_top_level_homebrew_casks_skip_then_error_propagates_second_failure() {
    let config = Config {
        homebrew_casks: Some(vec![
            HomebrewCaskConfig {
                name: Some("skipped-cask".to_string()),
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("homebrew-cask-tap".to_string()),
                    ..Default::default()
                }),
                skip_upload: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
            HomebrewCaskConfig {
                name: Some("broken-cask".to_string()),
                repository: None,
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no repository config"),
        "second entry's bail must propagate; got: {msg}"
    );
    assert!(
        msg.contains("broken-cask"),
        "bail must cite the second cask, not the first; got: {msg}"
    );
    assert!(
        !msg.contains("skipped-cask"),
        "first (skipped) entry must not appear in error; got: {msg}"
    );
}

// ===========================================================================
// publish_top_level_homebrew_casks — the live clone → write → commit → push
// path (beyond the early-exit / render-bail guards the tests above cover),
// driven against a LOCAL bare git remote (no network). PR submission is left
// disabled so `maybe_submit_pr` returns before any `gh`/API transport.
// ===========================================================================

/// Seed a bare "cask tap fork" repo with one commit on `branch`; the publish
/// path clones it (a plain `git clone <localpath>`), writes the cask `.rb`,
/// commits, and pushes back here. Returns `(bare_url, holder)`.
fn make_bare_cask_tap(branch: &str) -> (String, tempfile::TempDir) {
    let bare = tempfile::tempdir().expect("bare tempdir");
    let seed = tempfile::tempdir().expect("seed tempdir");
    let git_ok =
        |dir: &std::path::Path, args: &[&str]| anodizer_core::test_helpers::git_test_ok(dir, args);
    git_ok(bare.path(), &["init", "--bare", "-b", branch]);
    git_ok(seed.path(), &["init", "-b", branch]);
    git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
    git_ok(seed.path(), &["config", "user.name", "T"]);
    git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.path().join("README"), "cask tap\n").unwrap();
    git_ok(seed.path(), &["add", "README"]);
    git_ok(seed.path(), &["commit", "-m", "seed"]);
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["remote", "add", "origin"])
                    .arg(bare.path())
                    .current_dir(seed.path());
                cmd
            },
            "git",
        )
        .status
        .success(),
        "git remote add origin failed"
    );
    git_ok(seed.path(), &["push", "-u", "origin", branch]);
    (bare.path().to_string_lossy().into_owned(), bare)
}

/// Build a version-carrying Context (`1.2.3`) with `homebrew_casks` set and a
/// single darwin Archive carrying url + sha256 metadata (so the cask renders).
fn cask_publish_ctx(cask: HomebrewCaskConfig) -> Context {
    let config = Config {
        project_name: "mytool".to_string(),
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("RawVersion", "1.2.3");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin.tar.gz",
        "d".repeat(64).as_str(),
    ));
    ctx
}

/// A cask whose `repository.git.url` clones from the local bare tap and pushes
/// back to `branch`; PR submission stays disabled.
fn local_cask_cfg(bare_url: &str, branch: &str) -> HomebrewCaskConfig {
    HomebrewCaskConfig {
        name: Some("mycask".to_string()),
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-cask-tap".to_string()),
            branch: Some(branch.to_string()),
            git: Some(anodizer_core::config::GitRepoConfig {
                url: Some(bare_url.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// FULL live publish: clone the local bare tap, write `Casks/mycask.rb`,
/// commit, and push the branch back — asserting the bare repo gained the cask
/// file with the rendered version and the run reports one applicable+pushed
/// cask.
#[test]
fn publish_top_level_homebrew_casks_live_clone_writes_and_pushes() {
    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut ctx = cask_publish_ctx(local_cask_cfg(&bare_url, "main"));

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("live cask publish must succeed against the local bare tap");

    assert!(got.pushed_any, "the cask branch must be pushed: {got:?}");
    assert_eq!(got.total, 1);
    assert_eq!(got.applicable, 1, "the darwin cask is applicable");

    let landed = anodizer_core::test_helpers::git_test_stdout(
        bare.path(),
        &["show", "main:Casks/mycask.rb"],
    );
    assert!(
        landed.contains("cask \"mycask\"") && landed.contains("version \"1.2.3\""),
        "the pushed cask must carry the rendered name+version; got:\n{landed}"
    );
    drop(bare);
}

/// A cask-level `if:` that renders falsy is skipped with the falsy-condition
/// status line — before any clone is attempted.
#[test]
fn publish_top_level_homebrew_casks_if_false_skips_with_status() {
    let mut cask = local_cask_cfg("unused://never-cloned", "main");
    cask.if_condition = Some("{{ eq .Version \"9.9.9\" }}".to_string());
    let mut ctx = cask_publish_ctx(cask);
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &log)
        .expect("a falsy `if` must skip cleanly, never clone");
    assert!(!got.pushed_any);
    assert_eq!(got.applicable, 0, "a skipped cask is not applicable");
    assert!(
        capture
            .all_messages()
            .iter()
            .any(|(_, m)| m.contains("skipped cask 'mycask'") && m.contains("`if` condition")),
        "the falsy-`if` skip must log its status line: {:?}",
        capture.all_messages()
    );
}

/// A non-default cask `directory:` emits the end-user-breakage warning before
/// the (here dry-run-short-circuited) upload.
#[test]
fn publish_top_level_homebrew_casks_non_default_directory_warns() {
    let mut cask = local_cask_cfg("unused://never-cloned", "main");
    cask.directory = Some("NotCasks".to_string());
    let mut ctx = cask_publish_ctx(cask);
    ctx.options.dry_run = true; // stop before the clone; the warn fires first.
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);

    super::publish_top_level_homebrew_casks(&mut ctx, &log)
        .expect("dry-run cask publish is a no-op");
    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("NotCasks") && m.contains("Casks")),
        "a non-default directory must warn about the homebrew-cask convention: {:?}",
        capture.warn_messages()
    );
}

/// A second identical publish against the already-updated tap has nothing to
/// commit ⇒ the `NoChanges` arm logs "already up to date" and reports not
/// pushed (the tap is idempotent across re-runs).
#[test]
fn publish_top_level_homebrew_casks_second_run_is_noop_no_changes() {
    let (bare_url, bare) = make_bare_cask_tap("main");

    // First publish lands the cask on `main`.
    let mut ctx1 = cask_publish_ctx(local_cask_cfg(&bare_url, "main"));
    let first = super::publish_top_level_homebrew_casks(&mut ctx1, &quiet_log())
        .expect("first publish lands the cask");
    assert!(first.pushed_any, "first run must push");

    // Second publish clones the now-current tap, writes identical bytes ⇒ no diff.
    let mut ctx2 = cask_publish_ctx(local_cask_cfg(&bare_url, "main"));
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);
    let second = super::publish_top_level_homebrew_casks(&mut ctx2, &log)
        .expect("second publish is a clean no-op");
    assert!(
        !second.pushed_any,
        "an identical re-publish must not push: {second:?}"
    );
    assert!(
        capture
            .all_messages()
            .iter()
            .any(|(_, m)| m.contains("already up to date")),
        "the NoChanges arm must log the up-to-date status: {:?}",
        capture.all_messages()
    );
    drop(bare);
}

/// A PR-enabled cask still pushes the branch and then routes through
/// `maybe_submit_pr` (recording its outcome). A `gh` stub forced absent makes
/// the PR transport resolve to the in-process no-credential fallback — no live
/// `gh pr create` / GitHub API call — while still exercising the
/// update_existing_pr eval + submit + record_publisher_outcome seam.
#[test]
#[serial_test::serial(path_env)]
fn publish_top_level_homebrew_casks_pr_enabled_pushes_and_records_outcome() {
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    let tools = FakeToolDir::new();
    tools.tool("gh").exit(1).install();
    let _guard = tools.activate();

    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut cask = local_cask_cfg(&bare_url, "main");
    if let Some(repo) = cask.repository.as_mut() {
        repo.pull_request = Some(anodizer_core::config::PullRequestConfig {
            enabled: Some(true),
            ..Default::default()
        });
    }
    let mut ctx = cask_publish_ctx(cask);

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("PR-enabled cask publish must push even when PR submission has no transport");
    assert!(
        got.pushed_any,
        "the cask branch must land regardless of PR submission: {got:?}"
    );
    let landed = anodizer_core::test_helpers::git_test_stdout(
        bare.path(),
        &["show", "main:Casks/mycask.rb"],
    );
    assert!(
        landed.contains("cask \"mycask\""),
        "the cask must be pushed to the tap; got:\n{landed}"
    );
    // The PR-submission seam ran: with `gh` absent and no token the transport
    // resolves to a no-credential outcome that `maybe_submit_pr` returns and
    // the loop records — distinguishing this from the PR-disabled path, which
    // records nothing.
    assert!(
        ctx.take_pending_outcome().is_some(),
        "the enabled-PR path must record a submission outcome"
    );
    drop(bare);
}

/// A cask carrying a versioned `alternative_names` entry (one with `@`) emits
/// an EXTRA `<alt>.rb` alongside the primary cask — the `brew install
/// myapp@1.2.3` pin file — and pushes both. Exercises the versioned-alt render
/// loop and its per-alt file write.
#[test]
fn publish_top_level_homebrew_casks_versioned_alt_writes_extra_rb() {
    let (bare_url, bare) = make_bare_cask_tap("main");
    let mut cask = local_cask_cfg(&bare_url, "main");
    cask.alternative_names = Some(vec!["mycask@1.2.3".to_string()]);
    let mut ctx = cask_publish_ctx(cask);

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("versioned-alt cask publish must succeed");
    assert!(got.pushed_any, "the cask branch must be pushed: {got:?}");

    let primary = anodizer_core::test_helpers::git_test_stdout(
        bare.path(),
        &["show", "main:Casks/mycask.rb"],
    );
    assert!(
        primary.contains("cask \"mycask\""),
        "the primary cask must land; got:\n{primary}"
    );
    let versioned = anodizer_core::test_helpers::git_test_stdout(
        bare.path(),
        &["show", "main:Casks/mycask@1.2.3.rb"],
    );
    assert!(
        versioned.contains("cask \"mycask@1.2.3\""),
        "the versioned pin cask must be written alongside the primary; got:\n{versioned}"
    );
    drop(bare);
}

/// The offline render-only path (`render_top_level_homebrew_casks`) emits the
/// primary cask body plus one extra body per versioned alt-name — the schema
/// validator's view, with no git/network.
#[test]
fn render_top_level_homebrew_casks_includes_versioned_alt_bodies() {
    let mut cask = local_cask_cfg("unused://render-only", "main");
    cask.alternative_names = Some(vec!["mycask@1.2.3".to_string()]);
    let ctx = cask_publish_ctx(cask);

    let bodies = super::render_top_level_homebrew_casks(&ctx, &quiet_log())
        .expect("render-only path must succeed");
    assert!(
        bodies.len() >= 2,
        "the versioned alt must add a second rendered body: {} bodies",
        bodies.len()
    );
    assert!(
        bodies.iter().any(|b| b.contains("cask \"mycask\"")),
        "the primary cask body must be present"
    );
    assert!(
        bodies.iter().any(|b| b.contains("cask \"mycask@1.2.3\"")),
        "the versioned alt body must be present"
    );
}

/// publish_cask: when the cask-level `skip_upload` is unset, the fallback
/// reads from the surrounding HomebrewConfig.skip_upload — so a tap-wide
/// `skip_upload: true` correctly short-circuits the standalone cask
/// publisher without requiring a redundant per-cask override.
#[test]
fn publish_cask_falls_back_to_homebrew_skip_upload() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        skip_upload: Some(StringOrBool::Bool(true)),
        cask: Some(HomebrewCaskConfig {
            // No cask-level skip_upload; fallback should consult hb_cfg.
            skip_upload: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    super::publish_cask(&mut ctx, "mytool", &quiet_log())
        .expect("hb skip_upload=true must short-circuit when cask skip_upload is None");
}

/// publish_cask: when no macOS artifact exists, the call into
/// `generate_cask_from_context` bails with an error that names the crate
/// — distinct from the earlier `cask: None` / `repository: None` bails,
/// which short-circuit before generation. Forces the explicit
/// `skip_upload: false` path so the skip guard does not intercept.
#[test]
fn publish_cask_no_macos_artifact_errors_with_crate_name() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig {
            // Both skip_upload values explicitly false to force the path
            // through generate_cask_from_context.
            skip_upload: Some(StringOrBool::Bool(false)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    // Only a Linux artifact present; macOS lookup fails.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool.tar.gz",
        "linuxsha",
    ));
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("homebrew cask:"),
        "error must carry homebrew-cask context; got: {msg}"
    );
    assert!(
        msg.contains("no macOS artifact"),
        "must surface no-macOS-artifact bail; got: {msg}"
    );
    assert!(
        msg.contains("mytool"),
        "must cite the crate name; got: {msg}"
    );
}

/// `split_alternative_names`: rendered entries containing `@` route to
/// the "versioned-file" bucket (one `.rb` per entry); entries without
/// `@` route to the "alias" bucket (inline `name "..."` directives).
#[test]
fn split_alternative_names_partitions_by_at_sign() {
    let rendered = vec![
        "myapp@1.2.3".to_string(),
        "myapp-alias".to_string(),
        "myapp".to_string(), // matches base — dropped
        "".to_string(),      // empty — dropped
    ];
    let (aliases, versioned) = super::cask::split_alternative_names(&rendered, "myapp");
    assert_eq!(aliases, vec!["myapp-alias".to_string()]);
    assert_eq!(versioned, vec!["myapp@1.2.3".to_string()]);
}

/// `render_alternative_names`: pass-through when no template
/// substitutions are present, and the template engine renders
/// `{{ ProjectName }}` against the configured `Context`. A failure in
/// the engine surfaces as `Err` so a typo in the template doesn't
/// silently produce a malformed Ruby cask file.
#[test]
fn render_alternative_names_runs_each_entry_through_template_engine() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );

    let entries = vec![
        "{{ ProjectName }}-stable".to_string(),
        "literal".to_string(),
    ];
    let rendered = super::cask::render_alternative_names(&ctx, &entries).expect("render");
    assert_eq!(
        rendered,
        vec!["myapp-stable".to_string(), "literal".to_string()]
    );
}

/// publish_cask: cask-level `name` override is independent of the crate
/// name — when set, downstream cask filename uses the override, but
/// generator-level bails surface `crate_name` so operators can still match
/// the failure back to the crate that owns the publisher config.
#[test]
fn publish_cask_name_override_does_not_mask_crate_in_errors() {
    let cfg = HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig {
            name: Some("renamed-cask".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    // No artifacts -> generate_cask_from_context bails citing crate_name.
    let err = super::publish_cask(&mut ctx, "mytool", &quiet_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mytool"),
        "crate name (not the renamed cask) is the operator-visible handle in errors; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// ruby_escape: user-config string values that contain `"` or `\` must be
// escaped so the rendered formula/cask stays syntactically valid Ruby.
// ---------------------------------------------------------------------------

/// Run `ruby -c` on rendered Ruby source, asserting exit 0. Gated on `ruby`
/// being on `PATH`; prints a visible skip marker and returns when absent so a
/// machine without ruby reports SKIP rather than a false PASS.
fn assert_ruby_syntax_ok(label: &str, source: &str) {
    match anodizer_core::tool_detect::runs("ruby") {
        anodizer_core::tool_detect::ToolProbe::Available => {}
        anodizer_core::tool_detect::ToolProbe::Unavailable => {
            eprintln!("SKIP {label}: ruby not on PATH; cannot run `ruby -c`");
            return;
        }
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            eprintln!("SKIP {label}: ruby probe failed ({e}); cannot run `ruby -c`");
            return;
        }
    }
    let dir = tempfile::tempdir().expect("create temp dir for ruby -c");
    let path = dir.path().join("artifact.rb");
    std::fs::write(&path, source).expect("write rendered ruby");
    let output = std::process::Command::new("ruby")
        .arg("-c")
        .arg(&path)
        .output()
        .expect("run ruby -c");
    assert!(
        output.status.success(),
        "{label}: rendered Ruby failed `ruby -c`:\nstderr: {}\n--- source ---\n{source}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A formula whose description/homepage carry embedded `"` and `\` renders
/// escaped string literals and stays valid Ruby; the install line keeps its
/// own quotes raw (not double-escaped).
#[test]
fn formula_ruby_escapes_user_values_and_passes_ruby_c() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: r#"the "best" \tool"#,
            license: "MIT",
        },
        &[(
            "darwin-amd64",
            "https://example.com/mytool-1.0.0-darwin-amd64.tar.gz",
            "sha256abc",
        )],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\", \"--version\"",
        },
    )
    .unwrap();

    // The embedded quote and backslash are escaped inside the desc literal.
    assert!(
        formula.contains(r#"desc "the \"best\" \\tool""#),
        "description should be ruby-escaped; got:\n{formula}"
    );
    // The install line's own quotes are raw Ruby — escaping them would break
    // valid output, so they must NOT be doubled.
    assert!(
        formula.contains(r#"bin.install "mytool""#),
        "install line must stay raw (not double-escaped); got:\n{formula}"
    );
    assert_ruby_syntax_ok("formula", &formula);
}

/// A cask whose name/display_name/homepage/description carry embedded `"` and
/// `\` renders escaped string literals and stays valid Ruby; a `depends_on`
/// Ruby fragment containing a `"` stays raw (not double-escaped).
#[test]
fn cask_ruby_escapes_user_values_and_passes_ruby_c() {
    use super::cask::CaskParams;
    let deps = vec![r#"cask: "other-app""#.to_string()];
    let params = CaskParams {
        name: "mytool",
        display_name: r#"My "Great" \Tool"#,
        alternative_names: &[],
        version: "1.0.0",
        sha256: "deadbeef",
        url: "https://example.com/mytool-1.0.0.dmg",
        url_extras: "",
        url_extras_indented: "",
        homepage: Some(r#"https://example.com/a"b\c"#),
        description: Some(r#"the "best" \tool"#),
        app: None,
        binaries: &[],
        caveats: None,
        zap_block: "",
        uninstall_block: "",
        custom_block: None,
        service: None,
        livecheck: None,
        manpages: &[],
        completions_bash: None,
        completions_zsh: None,
        completions_fish: None,
        depends_on: &deps,
        conflicts_with: &[],
        preflight: None,
        postflight: None,
        uninstall_preflight: None,
        uninstall_postflight: None,
        platforms: Vec::new(),
        generate_completions: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();

    assert!(
        cask.contains(r#"name "My \"Great\" \\Tool""#),
        "display_name should be ruby-escaped; got:\n{cask}"
    );
    assert!(
        cask.contains(r#"desc "the \"best\" \\tool""#),
        "description should be ruby-escaped; got:\n{cask}"
    );
    // The depends_on directive is a raw Ruby fragment — its inner quotes must
    // pass through unescaped.
    assert!(
        cask.contains(r#"depends_on cask: "other-app""#),
        "depends_on fragment must stay raw (not double-escaped); got:\n{cask}"
    );
    assert_ruby_syntax_ok("cask", &cask);
}

/// `render_additional_url_params` escapes user values spliced into `verified`,
/// `referer`, `user_agent`, header, cookies, and data string literals; the
/// whole `url "…"` continuation renders to valid Ruby. The `using:` symbol is
/// raw Ruby and stays unescaped.
#[test]
fn url_params_ruby_escape_passes_ruby_c() {
    use anodizer_core::config::HomebrewCaskURL;
    use std::collections::HashMap;

    let mut cookies = HashMap::new();
    cookies.insert(r#"ck"y"#.to_string(), r#"v\al"#.to_string());
    let mut data = HashMap::new();
    data.insert(r#"d"k"#.to_string(), r#"d\v"#.to_string());
    let u = HomebrewCaskURL {
        template: None,
        verified: Some(r#"example.com/a"b\c"#.to_string()),
        using: Some(":homebrew_curl".to_string()),
        cookies: Some(cookies),
        referer: Some(r#"https://r"ef\er"#.to_string()),
        headers: Some(vec![r#"X-Tok: a"b\c"#.to_string()]),
        user_agent: Some(r#"Agent "1.0"\x"#.to_string()),
        data: Some(data),
    };
    let extras = super::cask::render_additional_url_params(&u, "      ");

    // Embedded quote/backslash are escaped inside each string literal.
    assert!(
        extras.contains(r#"verified: "example.com/a\"b\\c""#),
        "verified should be ruby-escaped; got:\n{extras}"
    );
    assert!(
        extras.contains(r#"referer: "https://r\"ef\\er""#),
        "referer should be ruby-escaped; got:\n{extras}"
    );
    assert!(
        extras.contains(r#"user_agent: "Agent \"1.0\"\\x""#),
        "user_agent should be ruby-escaped; got:\n{extras}"
    );
    assert!(
        extras.contains(r#""X-Tok: a\"b\\c""#),
        "header should be ruby-escaped; got:\n{extras}"
    );
    assert!(
        extras.contains(r#""ck\"y" => "v\\al""#),
        "cookie key/value should be ruby-escaped; got:\n{extras}"
    );
    assert!(
        extras.contains(r#""d\"k" => "d\\v""#),
        "data key/value should be ruby-escaped; got:\n{extras}"
    );
    // The `using:` symbol is raw Ruby — passes through verbatim.
    assert!(
        extras.contains("using: :homebrew_curl"),
        "using symbol must stay raw; got:\n{extras}"
    );

    // Splice the continuation into a real `url "…"` line and validate the
    // whole cask with `ruby -c`.
    let source = format!(
        "cask \"x\" do\n  version \"1.0\"\n  sha256 \"deadbeef\"\n  url \"https://e.com/x.dmg\"{extras}\n  name \"X\"\nend\n"
    );
    assert_ruby_syntax_ok("url_params", &source);
}

/// `render_generate_completions` escapes the executable, args, and `base_name`
/// string literals; the directive renders to valid Ruby. Shell symbols stay
/// raw.
#[test]
fn generate_completions_ruby_escape_passes_ruby_c() {
    use anodizer_core::config::HomebrewCaskGeneratedCompletions;

    let g = HomebrewCaskGeneratedCompletions {
        executable: Some(r#"bin/my"app\x"#.to_string()),
        args: Some(vec![r#"comp"le\tions"#.to_string()]),
        base_name: Some(r#"my"app\x"#.to_string()),
        shell_parameter_format: Some("cobra".to_string()),
        shells: Some(vec!["bash".to_string(), "zsh".to_string()]),
    };
    let directive = super::cask::render_generate_completions(&g).expect("directive");

    assert!(
        directive.contains(r#""bin/my\"app\\x""#),
        "executable should be ruby-escaped; got:\n{directive}"
    );
    assert!(
        directive.contains(r#""comp\"le\\tions""#),
        "arg should be ruby-escaped; got:\n{directive}"
    );
    assert!(
        directive.contains(r#"base_name: "my\"app\\x""#),
        "base_name should be ruby-escaped; got:\n{directive}"
    );
    // Known shell_parameter_format renders as a symbol — raw, not escaped.
    assert!(
        directive.contains("shell_parameter_format: :cobra"),
        "known format must render as a raw symbol; got:\n{directive}"
    );

    let source = format!("cask \"x\" do\n  version \"1.0\"\n  {directive}\nend\n");
    assert_ruby_syntax_ok("generate_completions", &source);
}

/// `build_depends_directives` / `build_conflicts_directives` escape the
/// package-name string literals inside `cask: "…"` / `formula: "…"`; the
/// resulting `depends_on` / `conflicts_with` lines render to valid Ruby.
#[test]
fn depends_conflicts_ruby_escape_passes_ruby_c() {
    use anodizer_core::config::{HomebrewCaskConflictEntry, HomebrewCaskDependencyEntry};

    let deps = vec![HomebrewCaskDependencyEntry {
        cask: Some(r#"oth"er\app"#.to_string()),
        formula: None,
    }];
    let conflicts = vec![HomebrewCaskConflictEntry {
        cask: None,
        formula: Some(r#"old"f\m"#.to_string()),
    }];
    let dep_dirs = super::formula::build_depends_directives(Some(&deps));
    let conf_dirs = super::formula::build_conflicts_directives(Some(&conflicts));

    assert_eq!(dep_dirs, vec![r#"cask: "oth\"er\\app""#.to_string()]);
    assert_eq!(conf_dirs, vec![r#"formula: "old\"f\\m""#.to_string()]);

    let source = format!(
        "cask \"x\" do\n  version \"1.0\"\n  depends_on {}\n  conflicts_with {}\n  name \"X\"\nend\n",
        dep_dirs[0], conf_dirs[0]
    );
    assert_ruby_syntax_ok("depends_conflicts", &source);
}

/// The `uninstall`/`zap` array renderer formats each entry with Rust's `{:?}`
/// debug, which already escapes `"`/`\` for a double-quoted literal — values
/// containing them stay valid Ruby and are NOT double-escaped.
#[test]
fn uninstall_zap_debug_format_stays_valid_ruby() {
    use anodizer_core::config::HomebrewCaskUninstall;

    let u = HomebrewCaskUninstall {
        launchctl: Some(vec![r#"com.ex"am\ple"#.to_string()]),
        quit: None,
        login_item: None,
        delete: None,
        trash: None,
    };
    let block = super::cask::render_uninstall_block(Some(&u));
    // Debug-format escapes the embedded quote and backslash exactly once.
    assert!(
        block.contains(r#""com.ex\"am\\ple""#),
        "debug-format should escape once (not double); got:\n{block}"
    );

    let source = format!("cask \"x\" do\n  version \"1.0\"\n  {block}\n  name \"X\"\nend\n");
    assert_ruby_syntax_ok("uninstall", &source);
}

/// Run `ruby -c` on Ruby source expected to be INVALID, asserting a non-zero
/// exit. Gated on `ruby` being on `PATH` (visible SKIP when absent). Used to
/// prove the escaping is load-bearing: the un-escaped equivalent must be
/// rejected by the same validator that accepts the escaped form.
fn assert_ruby_syntax_err(label: &str, source: &str) {
    match anodizer_core::tool_detect::runs("ruby") {
        anodizer_core::tool_detect::ToolProbe::Available => {}
        anodizer_core::tool_detect::ToolProbe::Unavailable => {
            eprintln!("SKIP {label}: ruby not on PATH; cannot run `ruby -c`");
            return;
        }
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            eprintln!("SKIP {label}: ruby probe failed ({e}); cannot run `ruby -c`");
            return;
        }
    }
    let dir = tempfile::tempdir().expect("create temp dir for ruby -c");
    let path = dir.path().join("artifact.rb");
    std::fs::write(&path, source).expect("write rendered ruby");
    let output = std::process::Command::new("ruby")
        .arg("-c")
        .arg(&path)
        .output()
        .expect("run ruby -c");
    assert!(
        !output.status.success(),
        "{label}: expected `ruby -c` to REJECT the un-escaped form, but it \
         passed — the escaping under test would be a no-op:\n--- source ---\n{source}",
    );
}

/// Discrimination test: `ruby_escape_str` is non-vacuous. For a value carrying
/// `"` and `\`, the escaped output differs from the naive un-escaped splice,
/// the naive form is REJECTED by `ruby -c`, and the escaped form is ACCEPTED.
/// This proves the escaping is load-bearing rather than a no-op the suite
/// would pass even if the filter were deleted.
#[test]
fn ruby_escape_is_load_bearing_not_a_noop() {
    use anodizer_core::template::ruby_escape_str;

    let raw = r#"the "best" \tool"#;
    let escaped = ruby_escape_str(raw);

    // The transform actually changes the string (would be identity if vacuous).
    assert_ne!(
        escaped, raw,
        "ruby_escape_str must transform a value with `\"`/`\\`"
    );
    assert_eq!(escaped, r#"the \"best\" \\tool"#);

    // The naive splice (no escaping) produces invalid Ruby.
    let naive = format!("cask \"x\" do\n  desc \"{raw}\"\nend\n");
    assert_ruby_syntax_err("naive-unescaped", &naive);

    // The escaped splice produces valid Ruby.
    let safe = format!("cask \"x\" do\n  desc \"{escaped}\"\nend\n");
    assert_ruby_syntax_ok("escaped", &safe);
}

/// The `bin.install` install-phase fragments built in `publish_formula.rs`
/// route binary/crate names through `ruby_escape_str` before splicing into
/// `"…"` literals, so a name carrying `"`/`\` would stay valid Ruby. Proves
/// the rename fragment's two sides are escaped INDIVIDUALLY (the structural
/// `" => "` quotes survive, yielding a valid Ruby hash argument), and that the
/// un-escaped name produces invalid Ruby — so the escaping is load-bearing.
#[test]
fn install_fragment_escapes_each_side_keeps_structure() {
    use anodizer_core::template::ruby_escape_str;

    let name = r#"my"app\x"#;
    let bin = r#"re"name\d"#;

    // Per-side escaping (the production approach): structural quotes survive,
    // each side's contents are escaped, and the result is a valid `name => bin`
    // hash argument.
    let fragment = format!("{}\" => \"{}", ruby_escape_str(name), ruby_escape_str(bin));
    let line = format!("bin.install \"{fragment}\"");
    assert_eq!(line, r#"bin.install "my\"app\\x" => "re\"name\\d""#);
    let source = format!("class T < Formula\n  def install\n    {line}\n  end\nend\n");
    assert_ruby_syntax_ok("install-fragment", &source);

    // No escaping at all (a name with a raw `"` ending the literal early) is
    // rejected by `ruby -c`, proving the escaping above is load-bearing.
    let naive_line = format!("bin.install \"{name}\" => \"{bin}\"");
    let broken = format!("class T < Formula\n  def install\n    {naive_line}\n  end\nend\n");
    assert_ruby_syntax_err("install-fragment-unescaped", &broken);
}

// -----------------------------------------------------------------------
// Formula renderer: previously-uncovered archive-layout and opts branches.
// -----------------------------------------------------------------------

/// A multi-archive macOS set with only an amd64 entry must emit the
/// "darwin_arm64 not supported" caveats fallback inside `on_macos do`
/// (the `has_only_amd64_macos` branch). The url/sha256 stay flat (no
/// `on_intel`/`on_arm` sub-blocks) because the macos set isn't arch-split.
#[test]
fn test_formula_macos_amd64_only_emits_arm_unsupported_caveats() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "monotool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            (
                "darwin-amd64",
                "https://example.com/monotool-darwin-amd64.tar.gz",
                "machash",
            ),
            (
                "linux-amd64",
                "https://example.com/monotool-linux-amd64.tar.gz",
                "linuxhash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"monotool\"",
            test: "system \"#{bin}/monotool\"",
        },
    )
    .unwrap();
    assert!(formula.contains("on_macos do"));
    assert!(formula.contains("if Hardware::CPU.arm?"));
    assert!(formula.contains("The darwin_arm64 architecture is not supported for the monotool"));
    assert!(formula.contains("machash"));
    // No arch sub-blocks for the amd64-only macOS set.
    assert!(!formula.contains("on_intel"));
    assert!(!formula.contains("on_arm do"));
}

/// A 32-bit linux ARM target (`armv7`) selects the
/// `Hardware::CPU.arm? && !Hardware::CPU.is_64_bit?` guard, distinct from
/// the 64-bit arm and intel guards.
#[test]
fn test_formula_linux_armv7_uses_32bit_arm_guard() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "pitool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            (
                "linux-armv7",
                "https://example.com/pitool-linux-armv7.tar.gz",
                "armv7hash",
            ),
            (
                "linux-amd64",
                "https://example.com/pitool-linux-amd64.tar.gz",
                "amd64hash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"pitool\"",
            test: "system \"#{bin}/pitool\"",
        },
    )
    .unwrap();
    assert!(formula.contains("if Hardware::CPU.arm? && !Hardware::CPU.is_64_bit?"));
    assert!(formula.contains("if Hardware::CPU.intel? && Hardware::CPU.is_64_bit?"));
    assert!(formula.contains("armv7hash"));
    assert!(formula.contains("amd64hash"));
}

/// Multi-archive entries whose platform tag is neither darwin nor linux
/// render through the `unknown_entries` branch as a flat
/// `# platform: <tag>` + url/sha256 trio, without any `on_macos`/`on_linux`
/// wrapper.
#[test]
fn test_formula_unknown_platform_emits_commented_flat_entry() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "xtool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            (
                "freebsd-amd64",
                "https://example.com/xtool-freebsd-amd64.tar.gz",
                "bsdhash",
            ),
            (
                "linux-amd64",
                "https://example.com/xtool-linux-amd64.tar.gz",
                "linuxhash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"xtool\"",
            test: "system \"#{bin}/xtool\"",
        },
    )
    .unwrap();
    assert!(formula.contains("# platform: freebsd-amd64"));
    assert!(formula.contains("url \"https://example.com/xtool-freebsd-amd64.tar.gz\""));
    assert!(formula.contains("sha256 \"bsdhash\""));
    // The linux entry still wraps in on_linux.
    assert!(formula.contains("on_linux do"));
    assert!(formula.contains("linuxhash"));
}

/// `download_strategy` + `custom_require` + `url_headers` all splice into
/// the `url "..."` line on the single-archive layout.
#[test]
fn test_formula_download_strategy_custom_require_and_headers() {
    let headers = vec![
        "Authorization: Bearer tok".to_string(),
        "X-Foo: bar".to_string(),
    ];
    let opts = FormulaOptions {
        download_strategy: Some("GitHubPrivateRepositoryReleaseDownloadStrategy"),
        custom_require: Some("private_strategy"),
        url_headers: Some(&headers),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "privtool",
            version: "2.0.0",
            description: "desc",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/privtool.tar.gz", "abc")],
        &super::formula::FormulaCode {
            install: "bin.install \"privtool\"",
            test: "system \"#{bin}/privtool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("require_relative \"private_strategy\""));
    assert!(
        formula.contains("using: GitHubPrivateRepositoryReleaseDownloadStrategy"),
        "download strategy must splice into url line\n{formula}"
    );
    assert!(formula.contains("headers: ["));
    assert!(formula.contains("\"Authorization: Bearer tok\","));
    assert!(formula.contains("\"X-Foo: bar\","));
}

/// `extra_install`, `post_install`, `custom_block`, `plist`, and `service`
/// opts each render their respective formula stanza.
#[test]
fn test_formula_extra_install_post_install_custom_block_plist_service() {
    let opts = FormulaOptions {
        extra_install: Some("chmod 0755, bin/\"extra\""),
        post_install: Some("system \"#{bin}/setup\""),
        custom_block: Some("  # custom ruby here"),
        plist: Some("      <plist>stub</plist>"),
        service: Some("    run [opt_bin/\"svc\"]"),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "fulltool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[("linux-amd64", "https://example.com/fulltool.tar.gz", "abc")],
        &super::formula::FormulaCode {
            install: "bin.install \"fulltool\"",
            test: "system \"#{bin}/fulltool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("chmod 0755, bin/\"extra\""));
    assert!(formula.contains("def post_install"));
    assert!(formula.contains("system \"#{bin}/setup\""));
    assert!(formula.contains("# custom ruby here"));
    assert!(formula.contains("plist_options startup: true"));
    assert!(formula.contains("def plist"));
    assert!(formula.contains("<plist>stub</plist>"));
    assert!(formula.contains("service do"));
    assert!(formula.contains("run [opt_bin/\"svc\"]"));
}

/// A dependency carrying an explicit `version` pin renders
/// `depends_on "name" => "version"` — the `version` branch of the dep
/// template, distinct from the `:optional` and bare branches.
#[test]
fn test_formula_dependency_with_version_pin() {
    use anodizer_core::config::HomebrewDependency;
    let deps = vec![HomebrewDependency {
        name: "openssl@3".to_string(),
        os: None,
        dep_type: None,
        version: Some("3.2.0".to_string()),
    }];
    let opts = FormulaOptions {
        dependencies: Some(&deps),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "vertool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"vertool\"",
            test: "system \"#{bin}/vertool\"",
        },
        &opts,
    )
    .unwrap();
    assert!(formula.contains("depends_on \"openssl@3\" => \"3.2.0\""));
}

/// macOS-only multi-archive set (intel + arm, no linux) emits
/// `depends_on :macos` (the `depends_on_macos` branch). The linux-only
/// counterpart emits `depends_on :linux`.
#[test]
fn test_formula_macos_only_multiarch_emits_depends_on_macos() {
    let macos_only = generate_formula(
        &super::formula::FormulaCore {
            name: "mactool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            ("darwin-amd64", "https://example.com/m-amd64.tar.gz", "h1"),
            ("darwin-arm64", "https://example.com/m-arm64.tar.gz", "h2"),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"mactool\"",
            test: "system \"#{bin}/mactool\"",
        },
    )
    .unwrap();
    assert!(macos_only.contains("depends_on :macos"));
    assert!(!macos_only.contains("depends_on :linux"));

    let linux_only = generate_formula(
        &super::formula::FormulaCore {
            name: "lintool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            ("linux-amd64", "https://example.com/l-amd64.tar.gz", "h3"),
            ("linux-arm64", "https://example.com/l-arm64.tar.gz", "h4"),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"lintool\"",
            test: "system \"#{bin}/lintool\"",
        },
    )
    .unwrap();
    assert!(linux_only.contains("depends_on :linux"));
    assert!(!linux_only.contains("depends_on :macos"));
}

/// The `license` field is guarded by `{% if license %}`: an empty license
/// omits the `license` stanza entirely rather than emitting `license ""`.
#[test]
fn test_formula_empty_license_omits_stanza() {
    let with_license = generate_formula(
        &super::formula::FormulaCore {
            name: "lictool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"lictool\"",
            test: "system \"#{bin}/lictool\"",
        },
    )
    .unwrap();
    assert!(with_license.contains("license \"MIT\""));

    let no_license = generate_formula(
        &super::formula::FormulaCore {
            name: "lictool",
            version: "1.0.0",
            description: "desc",
            license: "",
        },
        &[],
        &super::formula::FormulaCode {
            install: "bin.install \"lictool\"",
            test: "system \"#{bin}/lictool\"",
        },
    )
    .unwrap();
    assert!(!no_license.contains("license \""));
}

// ===========================================================================
// Template-rendering of user-supplied config string fields: a value carrying
// `{{ .Tag }}` must reach the rendered manifest resolved, never literal. These
// drive the CALLER-level entry points (the ones that hold a real Context +
// StageLogger), matching how `resolve_homebrew_metadata` renders description /
// homepage / license. Each asserts the resolved token IS present and the raw
// `{{` delimiter is NOT.
// ===========================================================================

/// Build a `Context` with a single `mytool` crate whose `publish.homebrew` is
/// `hb_cfg`, a darwin+linux archive pair so the formula/cask have artifacts to
/// point at, and `Tag` set to a resolvable value.
fn rendered_field_ctx(hb_cfg: HomebrewConfig) -> Context {
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                homebrew: Some(hb_cfg),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-amd64.tar.gz",
        "sha_darwin_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux-amd64.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool-1.2.3-linux-amd64.tar.gz",
        "sha_linux_amd64",
    ));
    ctx
}

/// Formula `caveats` / `custom_require` / `custom_block` / `plist` / `service`
/// carrying `{{ .Tag }}` are rendered through the template engine before they
/// reach the formula body.
#[test]
fn formula_string_fields_are_template_rendered() {
    let hb_cfg = HomebrewConfig {
        caveats: Some("Installed {{ .Tag }} — run `mytool init`.".to_string()),
        custom_require: Some("lib_{{ .Tag }}".to_string()),
        custom_block: Some("# build {{ .Tag }}".to_string()),
        plist: Some("<string>{{ .Tag }}</string>".to_string()),
        service: Some("run [opt_bin/\"mytool\", \"--tag={{ .Tag }}\"]".to_string()),
        ..Default::default()
    };
    let ctx = rendered_field_ctx(hb_cfg);
    let rendered =
        super::publish_formula::render_homebrew_formula_for_crate(&ctx, "mytool", &test_log())
            .expect("formula render")
            .expect("formula not skipped");
    let f = rendered.formula;
    assert!(
        f.contains("v1.2.3"),
        "a formula string field did not resolve `{{{{ .Tag }}}}`:\n{f}"
    );
    assert!(
        !f.contains("{{"),
        "formula carries an unrendered template delimiter:\n{f}"
    );
}

/// Per-crate cask `homepage` / `description` / `caveats` / `custom_block` /
/// `app` / `service` carrying `{{ .Tag }}` are rendered before reaching the
/// cask body. Drives `generate_cask_from_context` (shared by the per-crate
/// publish + standalone cask paths).
#[test]
fn per_crate_cask_string_fields_are_template_rendered() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        homepage: Some("https://example.com/{{ .Tag }}".to_string()),
        description: Some("mytool {{ .Tag }}".to_string()),
        caveats: Some("see {{ .Tag }}".to_string()),
        custom_block: Some("# {{ .Tag }}".to_string()),
        app: Some("MyTool {{ .Tag }}.app".to_string()),
        service: Some("run [opt_bin/\"mytool\", \"--tag={{ .Tag }}\"]".to_string()),
        ..Default::default()
    };
    let ctx = rendered_field_ctx(HomebrewConfig::default());
    let hb_cfg = HomebrewConfig::default();
    let result = super::cask_scope::generate_cask_from_context(
        &ctx,
        "mytool",
        &hb_cfg,
        &cask_cfg,
        &test_log(),
    )
    .expect("cask render");
    let c = result.content;
    assert!(
        c.contains("v1.2.3"),
        "a per-crate cask string field did not resolve `{{{{ .Tag }}}}`:\n{c}"
    );
    // `service` specifically must carry its resolved value.
    assert!(
        c.contains("--tag=v1.2.3"),
        "per-crate cask `service` did not resolve `{{{{ .Tag }}}}`:\n{c}"
    );
    assert!(
        !c.contains("{{"),
        "per-crate cask carries an unrendered template delimiter:\n{c}"
    );
}

/// Top-level `homebrew_casks:` `homepage` / `description` / `caveats` /
/// `custom_block` carrying `{{ .Tag }}` are rendered before reaching the cask
/// body. This is a DISTINCT path (`render_top_level_cask_inner` calls
/// `generate_cask` directly, not via `generate_cask_from_context`).
#[test]
fn top_level_cask_string_fields_are_template_rendered() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        homepage: Some("https://example.com/{{ .Tag }}".to_string()),
        description: Some("mytool {{ .Tag }}".to_string()),
        caveats: Some("see {{ .Tag }}".to_string()),
        custom_block: Some("# {{ .Tag }}".to_string()),
        app: Some("MyTool {{ .Tag }}.app".to_string()),
        service: Some("run [opt_bin/\"mytool\", \"--tag={{ .Tag }}\"]".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-amd64.tar.gz",
        "sha_darwin_amd64",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;
    assert!(
        c.contains("v1.2.3"),
        "a top-level cask string field did not resolve `{{{{ .Tag }}}}`:\n{c}"
    );
    // `service` specifically must carry its resolved value in this distinct path.
    assert!(
        c.contains("--tag=v1.2.3"),
        "top-level cask `service` did not resolve `{{{{ .Tag }}}}`:\n{c}"
    );
    assert!(
        !c.contains("{{"),
        "top-level cask carries an unrendered template delimiter:\n{c}"
    );
}

/// Top-level `homebrew_casks:` with BOTH `darwin/amd64` and `darwin/arm64`
/// artifacts must emit a per-arch `on_intel` / `on_arm` cask body so each
/// Mac architecture downloads its own binary. A single flat `url` would ship
/// one architecture's binary to all Mac users (the other arch's `brew install`
/// then yields a binary that won't run).
#[test]
fn top_level_cask_dual_darwin_arch_emits_per_arch_blocks() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-amd64.tar.gz",
        "sha_darwin_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;
    assert!(
        c.contains("on_intel do") && c.contains("on_arm do"),
        "dual-arch top-level cask must emit on_intel + on_arm blocks:\n{c}"
    );
    // Each arch's URL must be present (each Mac downloads its own binary).
    assert!(
        c.contains("mytool-#{version}-darwin-amd64.tar.gz")
            || c.contains("mytool-1.2.3-darwin-amd64.tar.gz"),
        "intel arch URL missing from dual-arch cask:\n{c}"
    );
    assert!(
        c.contains("mytool-#{version}-darwin-arm64.tar.gz")
            || c.contains("mytool-1.2.3-darwin-arm64.tar.gz"),
        "arm arch URL missing from dual-arch cask:\n{c}"
    );
    // Each arch's own sha256 must be present.
    assert!(
        c.contains("sha_darwin_amd64"),
        "intel sha256 missing from dual-arch cask:\n{c}"
    );
    assert!(
        c.contains("sha_darwin_arm64"),
        "arm sha256 missing from dual-arch cask:\n{c}"
    );
    // The flat single-url stanza must NOT appear once per-arch blocks are used.
    assert!(
        !c.contains("\n  url \""),
        "dual-arch cask must not also emit a flat top-level url stanza:\n{c}"
    );
}

/// A genuinely single-arch top-level cask (only one macOS artifact) must keep
/// the flat single-`url` body — no empty `on_arm` / `on_intel` wrappers.
#[test]
fn top_level_cask_single_darwin_arch_keeps_flat_url() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;
    assert!(
        !c.contains("on_intel do") && !c.contains("on_arm do"),
        "single-arch top-level cask must NOT emit on_intel/on_arm wrappers:\n{c}"
    );
    assert!(
        c.contains("  url \""),
        "single-arch top-level cask must emit a flat url stanza:\n{c}"
    );
    assert!(
        c.contains("sha_darwin_arm64"),
        "single-arch cask must carry its sha256:\n{c}"
    );
}

/// `universal_binaries.replace: true` removes the per-arch darwin Archives
/// from the catalog, leaving only the lipo'd `darwin-universal` Archive. That
/// synthetic target has architecture `all`, which fills neither the `intel`
/// nor the `arm` slot, so the top-level cask falls back to a FLAT `url` /
/// `sha256` pointing at the universal binary — with no `on_arm` / `on_intel`
/// wrappers (and no `on_macos` arch-split block).
#[test]
fn top_level_cask_universal_replace_falls_back_to_flat_url() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    // Only the universal Archive remains — `replace: true` dropped the
    // per-arch `darwin-amd64` / `darwin-arm64` Archives from the catalog.
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-universal.tar.gz",
        "darwin-universal",
        "https://e.com/mytool-1.2.3-darwin-universal.tar.gz",
        "sha_darwin_universal",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;
    assert!(
        !c.contains("on_arm do") && !c.contains("on_intel do"),
        "universal-only cask must NOT emit per-arch on_arm/on_intel blocks:\n{c}"
    );
    assert!(
        !c.contains("on_macos do"),
        "universal-only cask must NOT emit an arch-split on_macos block:\n{c}"
    );
    assert!(
        c.contains("  url \"https://e.com/mytool-#{version}-darwin-universal.tar.gz\""),
        "universal-only cask must emit a flat url pointing at the universal binary:\n{c}"
    );
    assert!(
        c.contains("  sha256 \"sha_darwin_universal\""),
        "universal-only cask must emit the universal binary's flat sha256:\n{c}"
    );
}

/// A release carrying BOTH darwin (amd64 + arm64) AND linux (amd64 + arm64)
/// archives renders an `on_macos` block and an `on_linux` block, each with its
/// own `on_arm` / `on_intel` sub-blocks carrying per-arch url + sha256. The OS
/// block order is deterministic (`on_linux` before `on_macos`, BTreeMap key
/// order) and the per-arch order matches the `arch_block` sort (`on_arm`
/// before `on_intel`).
#[test]
fn top_level_cask_darwin_and_linux_coexist_with_deterministic_order() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-amd64.tar.gz",
        "x86_64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-amd64.tar.gz",
        "sha_darwin_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-arm64.tar.gz",
        "sha_darwin_arm64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux-amd64.tar.gz",
        "x86_64-unknown-linux-gnu",
        "https://e.com/mytool-1.2.3-linux-amd64.tar.gz",
        "sha_linux_amd64",
    ));
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-linux-arm64.tar.gz",
        "aarch64-unknown-linux-gnu",
        "https://e.com/mytool-1.2.3-linux-arm64.tar.gz",
        "sha_linux_arm64",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;

    // Both OS blocks present, each exactly once.
    assert_eq!(
        c.matches("on_macos do").count(),
        1,
        "expected exactly one on_macos block:\n{c}"
    );
    assert_eq!(
        c.matches("on_linux do").count(),
        1,
        "expected exactly one on_linux block:\n{c}"
    );
    // Each OS carries both arch sub-blocks (so on_arm/on_intel each appear twice).
    assert_eq!(
        c.matches("on_arm do").count(),
        2,
        "expected on_arm under both on_macos and on_linux:\n{c}"
    );
    assert_eq!(
        c.matches("on_intel do").count(),
        2,
        "expected on_intel under both on_macos and on_linux:\n{c}"
    );
    // Every arch carries its own url + sha256.
    for sha in [
        "sha_darwin_amd64",
        "sha_darwin_arm64",
        "sha_linux_amd64",
        "sha_linux_arm64",
    ] {
        assert!(c.contains(sha), "cask dropped the {sha} arch entry:\n{c}");
    }

    // OS-block order is deterministic: on_linux precedes on_macos (BTreeMap
    // key order over "linux" < "macos").
    let linux_pos = c.find("on_linux do").expect("on_linux present");
    let macos_pos = c.find("on_macos do").expect("on_macos present");
    assert!(
        linux_pos < macos_pos,
        "on_linux must precede on_macos (deterministic OS order):\n{c}"
    );

    // Within each OS block, per-arch order matches the arch_block sort:
    // on_arm precedes on_intel ("arm" < "intel"). The on_macos block is the
    // tail (it follows on_linux), so its arch sub-blocks live after macos_pos.
    let macos_block = &c[macos_pos..];
    let arm_in_macos = macos_block.find("on_arm do").expect("on_arm in macos");
    let intel_in_macos = macos_block.find("on_intel do").expect("on_intel in macos");
    assert!(
        arm_in_macos < intel_in_macos,
        "within on_macos, on_arm must precede on_intel (arch_block sort):\n{c}"
    );
}

/// A watchOS archive (os="darwin" via map_target's broad apple rule) must NOT
/// win the `on_macos`/`on_arm` cask slot — only the genuine `*-apple-darwin`
/// arm64 binary is `brew`-installable. Guards the `build_cask_platform_blocks`
/// `is_macos` gate against the failure-hiding emission.
#[test]
fn top_level_cask_excludes_watchos_from_macos_block() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        homebrew_casks: Some(vec![cask_cfg.clone()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    // Genuine macOS arm64 build...
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-darwin-arm64.tar.gz",
        "aarch64-apple-darwin",
        "https://e.com/mytool-1.2.3-darwin-arm64.tar.gz",
        "sha_real_darwin",
    ));
    // ...and a watchOS arm64 build that map_target also classifies os="darwin".
    ctx.artifacts.add(art_with_url_sha(
        ArtifactKind::Archive,
        "mytool-watchos-arm64.tar.gz",
        "aarch64-apple-watchos",
        "https://e.com/mytool-1.2.3-watchos-arm64.tar.gz",
        "sha_watchos",
    ));
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let c = rendered.content;
    assert!(
        c.contains("sha_real_darwin"),
        "genuine macOS arm64 binary must be in the cask:\n{c}"
    );
    assert!(
        !c.contains("sha_watchos"),
        "watchOS archive must never reach the on_macos cask block:\n{c}"
    );
}
