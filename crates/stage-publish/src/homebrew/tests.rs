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

/// Regression for parity with GoReleaser's `ErrNoArchivesFound`: empty
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
// Task 4C: Additional behavior tests -- config fields actually do things
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
    let msg = render_commit_msg(None, "mytool", "1.2.3", "formula");
    assert_eq!(msg, "Brew formula update for mytool version 1.2.3");
}

#[test]
fn test_render_commit_msg_custom_template() {
    let msg = render_commit_msg(
        Some("release: {{ name }} v{{ version }}"),
        "mytool",
        "2.0.0",
        "formula",
    );
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
/// AFTER the `postflight` stanza. Upstream commit bb9062f / GR issue #5958.
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
    // GR canonical shape: `uninstall launchctl: [...], quit: [...]` with
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
