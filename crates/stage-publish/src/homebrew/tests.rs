#![allow(clippy::field_reassign_with_default)]

use super::commit_msg::render_commit_msg;
use super::formula::{FormulaOptions, generate_formula, generate_formula_with_opts};

#[test]
fn test_generate_formula() {
    let formula = generate_formula(
        "cfgd",
        "1.0.0",
        &[(
            "darwin-amd64",
            "https://example.com/cfgd-1.0.0-darwin-amd64.tar.gz",
            "sha256abc",
        )],
        "Declarative config management",
        "MIT",
        "bin.install \"cfgd\"",
        "system \"#{bin}/cfgd\", \"--version\"",
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
        "my-tool",
        "2.0.0",
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
        "A tool",
        "Apache-2.0",
        "bin.install \"my-tool\"",
        "system \"#{bin}/my-tool\", \"--version\"",
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
        "cfgd-core",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"cfgd-core\"",
        "system \"#{bin}/cfgd-core\", \"--version\"",
    )
    .unwrap();
    assert!(formula.contains("class CfgdCore < Formula"));
}

#[test]
fn test_generate_formula_multi_arch_grouped() {
    // darwin-amd64 and darwin-arm64 must produce a single on_macos block
    // containing on_intel and on_arm sub-blocks.
    let formula = generate_formula(
        "mytool",
        "3.0.0",
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
        "My tool",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\", \"--version\"",
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
            "anodizer",
            "3.2.1",
            &[(
                "darwin-arm64",
                "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-darwin-arm64.tar.gz",
                "aabbccdd11223344",
            )],
            "Release automation for Rust projects",
            "Apache-2.0",
            "bin.install \"anodizer\"",
            "system \"#{bin}/anodizer\", \"--version\"",
        ).unwrap();

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
        "my-cli",
        "2.0.0",
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
        "A CLI tool",
        "MIT",
        "bin.install \"my-cli\"",
        "system \"#{bin}/my-cli\", \"--version\"",
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
        "empty-tool",
        "0.1.0",
        &[],
        "An empty tool",
        "MIT",
        "bin.install \"empty-tool\"",
        "system \"#{bin}/empty-tool\", \"--help\"",
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
            "complex-app",
            "1.0.0",
            &[("linux-amd64", "https://example.com/app.tar.gz", "hash123")],
            "Complex app",
            "MIT",
            "bin.install \"complex-app\"\nman1.install \"complex-app.1\"",
            "system \"#{bin}/complex-app\", \"--version\"\nassert_match \"complex-app\", shell_output(\"#{bin}/complex-app --help\")",
        ).unwrap();

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
        "myapp",
        "1.0.0",
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
        "My app",
        "MIT",
        "bin.install \"myapp\"",
        "system \"#{bin}/myapp\", \"--version\"",
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
        "simple",
        "1.0.0",
        &[("linux-amd64", "https://example.com/simple.tar.gz", "abc123")],
        "Simple tool",
        "MIT",
        "bin.install \"simple\"",
        "system \"#{bin}/simple\"",
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
        "my-cool-tool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"my-cool-tool\"",
        "system \"#{bin}/my-cool-tool\"",
    )
    .unwrap();
    assert!(formula.contains("class MyCoolTool < Formula"));
}

#[test]
fn test_formula_class_name_at_sign() {
    let formula = generate_formula(
        "node@20",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"node\"",
        "system \"#{bin}/node\"",
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
        "c++check",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"cppcheck\"",
        "system \"#{bin}/cppcheck\"",
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
        "my.tool.app",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"my.tool.app\"",
        "system \"#{bin}/my.tool.app\"",
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
        "mytool",
        "1.0.0",
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "mytool",
        "1.0.0",
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        "desc",
        "MIT",
        "bin.install \"mytool\"",
        "system \"#{bin}/mytool\"",
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
        "my-custom-name",
        "1.0.0",
        &[("linux-amd64", "https://example.com/a.tar.gz", "abc")],
        "desc",
        "MIT",
        "bin.install \"my-custom-name\"",
        "system \"#{bin}/my-custom-name\"",
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
