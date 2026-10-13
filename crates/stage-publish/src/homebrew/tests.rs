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

    // One top-level install serves every platform: `brew style` refuses a
    // method defined inside a block.
    assert!(
        formula.contains("\n  def install\n"),
        "multi-arch formula should have a top-level def install, got:\n{}",
        formula
    );
    assert_eq!(
        formula.matches("def install").count(),
        1,
        "the formula should define install once, got:\n{}",
        formula
    );
    // Linux picks its download with the same `on_arm` / `on_intel` blocks as
    // macOS: a `Hardware::CPU` condition reads the machine running brew, so
    // `brew readall` and every simulated platform would see the host's CPU.
    assert!(!formula.contains("Hardware::CPU"), "got:\n{formula}");
    assert!(
        formula.contains(
            "  on_linux do\n    on_arm do\n      url \"https://example.com/my-cli-2.0.0-linux-arm64.tar.gz\"\n      sha256 \"sha_linux_arm64\"\n    end\n    on_intel do\n      url \"https://example.com/my-cli-2.0.0-linux-amd64.tar.gz\"\n      sha256 \"sha_linux_amd64\"\n    end\n  end\n"
        ),
        "got:\n{formula}"
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

    // A macOS-only formula takes the Apple Silicon download as its top-level
    // url, which every platform `brew readall` loads then has, and overrides
    // it for Intel Macs inside `on_macos`.
    assert!(
        formula.contains(
            "  url \"https://example.com/myapp-darwin-arm64.tar.gz\"\n  version \"1.0.0\"\n  sha256 \"hash_arm\"\n"
        ),
        "{formula}"
    );
    assert!(
        formula.contains(
            "  depends_on :macos\n\n  on_macos do\n    on_intel do\n      url \"https://example.com/myapp-darwin-amd64.tar.gz\"\n      sha256 \"hash_intel\"\n    end\n  end\n"
        ),
        "{formula}"
    );
    assert!(!formula.contains("on_arm"), "{formula}");
    assert!(!formula.contains("on_linux"), "{formula}");
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
        &test_log(),
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
        &test_log(),
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
        &test_log(),
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
        &test_log(),
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
        &test_log(),
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

/// Homebrew compares lower-cased names and orders dependencies by type, so
/// `Azure/kubelogin/kubelogin` sorts after `aws-iam-authenticator`, a
/// `build` dependency leads and an `optional` one trails whatever its name.
#[test]
fn formula_dependencies_sort_by_type_then_name_without_regard_to_case() {
    use anodizer_core::config::HomebrewDependency;
    let dep_on = |name: &str, dep_type: Option<&str>, os: Option<&str>| HomebrewDependency {
        name: name.to_string(),
        os: os.map(str::to_string),
        dep_type: dep_type.map(str::to_string),
        version: None,
    };
    let dep = |name: &str, dep_type: Option<&str>| dep_on(name, dep_type, None);
    let deps = vec![
        dep("kubernetes-cli", None),
        dep("Azure/kubelogin/kubelogin", None),
        dep("aaa-optional", Some("optional")),
        dep("aws-iam-authenticator", None),
        dep("zzz-build", Some("build")),
        dep("mmm-test", Some("test")),
        dep("bbb-recommended", Some("recommended")),
        dep_on("Zed-mac", None, Some("mac")),
        dep_on("yyy-mac-build", Some("build"), Some("mac")),
        dep_on("apple-mac", None, Some("mac")),
        dep_on("Lin-runtime", None, Some("linux")),
        dep_on("aaa-linux-optional", Some("optional"), Some("linux")),
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
        &test_log(),
    )
    .unwrap();
    let rendered: Vec<&str> = formula
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("depends_on \"")
                || line.starts_with("on_macos")
                || line.starts_with("on_linux")
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            "depends_on \"zzz-build\" => :build",
            "depends_on \"mmm-test\" => :test",
            "depends_on \"aws-iam-authenticator\"",
            "depends_on \"Azure/kubelogin/kubelogin\"",
            "depends_on \"kubernetes-cli\"",
            "depends_on \"bbb-recommended\" => :recommended",
            "depends_on \"aaa-optional\" => :optional",
            "on_macos do",
            "depends_on \"yyy-mac-build\" => :build",
            "depends_on \"apple-mac\"",
            "depends_on \"Zed-mac\"",
            "on_linux do",
            "depends_on \"Lin-runtime\"",
            "depends_on \"aaa-linux-optional\" => :optional",
        ],
        "formula:\n{formula}"
    );
}

/// A formula whose archives are already split per OS puts an OS-scoped
/// dependency inside THAT `on_macos` / `on_linux` block: brew's audit refuses
/// a formula holding two blocks of one OS.
#[test]
fn os_dependencies_join_the_existing_per_os_block() {
    use anodizer_core::config::HomebrewDependency;
    let dep_on = |name: &str, os: &str| HomebrewDependency {
        name: name.to_string(),
        os: Some(os.to_string()),
        dep_type: None,
        version: None,
    };
    let deps = vec![dep_on("mac-only", "mac"), dep_on("linux-only", "linux")];
    let opts = FormulaOptions {
        dependencies: Some(&deps),
        ..Default::default()
    };
    let amd64 = (
        "darwin-amd64",
        "https://example.com/mytool-darwin-amd64.tar.gz",
        "aaaa",
    );
    let arm64 = (
        "darwin-arm64",
        "https://example.com/mytool-darwin-arm64.tar.gz",
        "bbbb",
    );
    let linux = (
        "linux-amd64",
        "https://example.com/mytool-linux-amd64.tar.gz",
        "cccc",
    );
    // Two macOS shapes render the block differently: one amd64 archive alone,
    // and one archive per architecture.
    for archives in [vec![amd64, linux], vec![amd64, arm64, linux]] {
        let formula = generate_formula_with_opts(
            &super::formula::FormulaCore {
                name: "mytool",
                version: "1.0.0",
                description: "desc",
                license: "MIT",
            },
            &archives,
            &super::formula::FormulaCode {
                install: "bin.install \"mytool\"",
                test: "system \"#{bin}/mytool\"",
            },
            &opts,
            &test_log(),
        )
        .unwrap();
        assert_eq!(
            formula.matches("on_macos do").count(),
            1,
            "formula:\n{formula}"
        );
        assert_eq!(
            formula.matches("on_linux do").count(),
            1,
            "formula:\n{formula}"
        );
        let mac_block =
            &formula[formula.find("on_macos do").unwrap()..formula.find("on_linux do").unwrap()];
        assert!(
            mac_block.contains("depends_on \"mac-only\""),
            "formula:\n{formula}"
        );
        assert!(!mac_block.contains("linux-only"), "formula:\n{formula}");
        let linux_block = &formula[formula.find("on_linux do").unwrap()..];
        assert!(
            linux_block.contains("depends_on \"linux-only\""),
            "formula:\n{formula}"
        );
        assert!(!linux_block.contains("mac-only"), "formula:\n{formula}");
    }
}

/// `type` renders as the Ruby symbol and wins over `version` on one entry.
#[test]
fn formula_dependency_type_takes_precedence_over_version() {
    use anodizer_core::config::HomebrewDependency;
    let deps = vec![HomebrewDependency {
        name: "pkg-config".to_string(),
        os: None,
        dep_type: Some("build".to_string()),
        version: Some(">= 0.29".to_string()),
    }];
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
        &test_log(),
    )
    .unwrap();
    assert!(
        formula.contains("depends_on \"pkg-config\" => :build\n"),
        "{formula}"
    );
    assert!(!formula.contains(">= 0.29"), "{formula}");
}

/// A cask lists its cask dependencies before its formula dependencies,
/// each group sorted by the lower-cased name with the names kept as written.
#[test]
fn cask_dependencies_sort_casks_then_formulas_without_regard_to_case() {
    use anodizer_core::config::HomebrewCaskDependencyEntry;
    let entry = |cask: Option<&str>, formula: Option<&str>| HomebrewCaskDependencyEntry {
        cask: cask.map(str::to_string),
        formula: formula.map(str::to_string),
    };
    let deps = vec![
        entry(None, Some("kubernetes-cli")),
        entry(Some("zotero"), None),
        entry(None, Some("Azure/kubelogin/kubelogin")),
        entry(Some("Example/tap/app"), None),
        entry(None, Some("aws-iam-authenticator")),
        entry(Some("alacritty"), None),
    ];
    assert_eq!(
        super::formula::build_depends_directives(Some(&deps)),
        vec![
            "cask: \"alacritty\"",
            "cask: \"Example/tap/app\"",
            "cask: \"zotero\"",
            "formula: \"aws-iam-authenticator\"",
            "formula: \"Azure/kubelogin/kubelogin\"",
            "formula: \"kubernetes-cli\"",
        ]
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
        &test_log(),
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
        &test_log(),
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
        &test_log(),
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
// Cask tests: sha256/url ordering, generate_completions
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
        depends_on_macos: None,
    }
}

/// Per-arch blocks emit `sha256` before `url`, the order `brew style` wants.
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

/// `render_generate_completions` lines its keyword arguments up under the
/// first positional argument, with their values in one column, which is the
/// layout `Layout/ArgumentAlignment` and `Layout/HashAlignment` accept.
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
        format!(
            "generate_completions_from_executable \"bin/myapp\", \"completions\",\n\
             {pad}base_name:              \"myapp\",\n\
             {pad}shell_parameter_format: :cobra,\n\
             {pad}shells:                 [:bash, :zsh, :fish, :pwsh]",
            pad = " ".repeat(39)
        )
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
        format!(
            "generate_completions_from_executable \"myapp\",\n{}shell_parameter_format: :cobra",
            " ".repeat(39)
        )
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

/// The `generate_completions_from_executable` stanza belongs to Homebrew's
/// artifact group, so it follows the completion files and precedes
/// `postflight`. Homebrew runs artifacts in a fixed order by kind, so this
/// placement does not change when the completions are generated.
#[test]
fn test_cask_generate_completions_renders_with_the_artifacts() {
    let mut params = empty_cask_params("test", "0.1.0");
    params.completions_fish = Some("test.fish");
    params.postflight = Some("system_command \"chmod\", args: [\"+x\", \"#{bin}/test\"]");
    params.generate_completions = render_generate_completions(&HomebrewCaskGeneratedCompletions {
        executable: Some("bin/myapp".to_string()),
        shell_parameter_format: Some("cobra".to_string()),
        ..Default::default()
    });
    let cask = generate_cask(&params).unwrap();
    let expected = format!(
        "  fish_completion \"test.fish\"\n\
         \x20 generate_completions_from_executable \"bin/myapp\",\n\
         {}shell_parameter_format: :cobra\n\
         \n\
         \x20 postflight do\n",
        " ".repeat(39)
    );
    assert!(cask.contains(&expected), "{cask}");
}

// ---------------------------------------------------------------------------
// cask `uninstall` / `zap` stanzas: one key per directive.
// ---------------------------------------------------------------------------

use anodizer_core::config::HomebrewCaskUninstall;

/// Every directive gets its own key, the keys line up under the first and
/// their values in one column, a one-element list renders as a bare string,
/// and a longer list is sorted the way `Cask/ArrayAlphabetization` checks.
#[test]
fn test_cask_zap_block_emits_each_directive_as_separate_key() {
    let zap_cfg = HomebrewCaskUninstall {
        launchctl: Some(vec!["com.example.daemon".to_string()]),
        quit: Some(vec!["com.example.app".to_string()]),
        login_item: Some(vec!["MyApp".to_string()]),
        delete: Some(vec!["/tmp/foo".to_string(), "/tmp/Bar".to_string()]),
        trash: Some(vec!["~/Library/MyApp".to_string()]),
    };
    assert_eq!(
        super::cask::render_zap_block(Some(&zap_cfg)),
        "zap launchctl:  \"com.example.daemon\",\n\
         \x20     quit:       \"com.example.app\",\n\
         \x20     login_item: \"MyApp\",\n\
         \x20     delete:     [\n\
         \x20       \"/tmp/Bar\",\n\
         \x20       \"/tmp/foo\",\n\
         \x20     ],\n\
         \x20     trash:      \"~/Library/MyApp\""
    );
}

/// A lone key shares the stanza's line, so its array elements are indented
/// from the start of that line, as RuboCop measures them.
#[test]
fn test_cask_zap_block_only_trash() {
    let zap_cfg = HomebrewCaskUninstall {
        trash: Some(vec!["~/Library/Foo".to_string(), "~/.foo".to_string()]),
        ..Default::default()
    };
    assert_eq!(
        super::cask::render_zap_block(Some(&zap_cfg)),
        "zap trash: [\n    \"~/.foo\",\n    \"~/Library/Foo\",\n  ]"
    );
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
fn test_cask_uninstall_block_uses_one_key_per_directive() {
    let u_cfg = HomebrewCaskUninstall {
        launchctl: Some(vec!["com.example.daemon".to_string()]),
        quit: Some(vec!["com.example.app".to_string()]),
        ..Default::default()
    };
    assert_eq!(
        super::cask::render_uninstall_block(Some(&u_cfg)),
        format!(
            "uninstall launchctl: \"com.example.daemon\",\n{}quit:      \"com.example.app\"",
            " ".repeat(12)
        )
    );
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
    assert!(
        cask.contains(
            "\n  zap launchctl: \"com.example.daemon\",\n      trash:     \"~/Library/MyApp\"\nend\n"
        ),
        "zap must be the last stanza, both keys aligned:\n{cask}"
    );
}

// ---------------------------------------------------------------------------
// cask `additional_url_params` (using, cookies, referer, headers,
// user_agent, data) renders on the `url` line.
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
    // Splices directly after the closing `"` of `url "..."`; the removed
    // `verified` stanza is never written.
    assert_eq!(
        extras,
        ",\n\
         \x20     using:      :homebrew_curl,\n\
         \x20     cookies:    {\n\
         \x20       \"session\" => \"deadbeef\",\n\
         \x20     },\n\
         \x20     referer:    \"https://example.com/\",\n\
         \x20     header:     [\"X-Auth: Bearer xyz\"],\n\
         \x20     user_agent: \"Mozilla/5.0\",\n\
         \x20     data:       {\n\
         \x20       \"user\" => \"alice\",\n\
         \x20     }"
    );
}

#[test]
fn test_render_additional_url_params_empty_returns_empty() {
    let url_cfg = HomebrewCaskURL::default();
    assert_eq!(
        super::cask::render_additional_url_params(&url_cfg, "      "),
        ""
    );
}

/// A cask whose only `url:` sub-field is the removed `verified:` renders no
/// kwargs at all — the stanza Homebrew dropped never reaches the file.
#[test]
fn cask_body_never_emits_verified() {
    let url_cfg = HomebrewCaskURL {
        verified: Some("example.com/".to_string()),
        ..Default::default()
    };
    let extras = super::cask::render_additional_url_params(&url_cfg, "      ");
    assert_eq!(extras, "");

    let mut params = empty_cask_params("test", "0.1.0");
    params.url_extras = &extras;
    let cask = generate_cask(&params).unwrap();
    assert!(
        !cask.contains("verified"),
        "the rendered cask must carry no verified stanza:\n{cask}"
    );
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
    // preferred-format bucket itself is ambiguous, so this must still error.
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

/// An `if: ""` on the cask imposes no gate, so the formula's real gate still
/// covers the cask. Reading the raw `Option` let the empty string win the
/// fallback and publish a cask the parent gate had ruled out.
#[test]
fn an_empty_cask_if_keeps_the_formulas_gate() {
    let cask = HomebrewCaskConfig {
        name: Some("mycask".to_string()),
        if_condition: Some(String::new()),
        ..Default::default()
    };
    let hb_cfg = HomebrewConfig {
        if_condition: Some("false".to_string()),
        cask: Some(cask.clone()),
        ..Default::default()
    };
    let ctx = hb_ctx(hb_cfg.clone(), false);
    assert!(
        super::publish_cask::cask_skip_gates_trip(
            &ctx,
            &hb_cfg,
            &cask,
            "mytool",
            &quiet_log(),
            true
        )
        .expect("the gate must evaluate"),
        "the formula's falsy `if` must still gate the cask"
    );

    // The cask's OWN gate still wins when it imposes one.
    let opted_in = HomebrewCaskConfig {
        if_condition: Some("true".to_string()),
        ..cask
    };
    assert!(
        !super::publish_cask::cask_skip_gates_trip(
            &ctx,
            &hb_cfg,
            &opted_in,
            "mytool",
            &quiet_log(),
            true
        )
        .expect("the gate must evaluate"),
        "a truthy cask `if` overrides the formula's"
    );
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
fn publish_to_homebrew_missing_repository_skips() {
    let cfg = HomebrewConfig {
        repository: None,
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    let pushed = super::publish_to_homebrew(&mut ctx, "mytool", &quiet_log())
        .expect("a tap-less entry must skip, not fail");
    assert!(!pushed);
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "homebrew");
    assert_eq!(events[0].label, "mytool");
    assert_eq!(events[0].reason, "repository.name is not set");
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
/// mentioning the filter knobs (ids / amd64_variant) and the CPUs a formula
/// serves. With no variant configured, the hint carries the `<default …>`
/// marker so the operator can tell a fallback apart from an explicit setting.
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
    assert!(
        msg.contains("a formula serves arm64, x86_64 and universal archives only"),
        "{msg}"
    );
    assert!(!msg.contains("arm_variant"), "{msg}");
}

/// publish_to_homebrew: when the variant selector IS configured, the
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
    assert!(msg.contains("amd64_variant=v3)"), "{msg}");
    assert!(!msg.contains("arm_variant"), "{msg}");
    assert!(!msg.contains("<default"), "{msg}");
}

/// publish_cask: no `publish.homebrew` and no `publish.homebrew_cask` => error.
#[test]
fn publish_cask_without_any_cask_errors() {
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
    assert!(msg.contains("no cask config"), "{msg}");
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
fn publish_cask_missing_repository_skips() {
    let cfg = HomebrewConfig {
        repository: None,
        cask: Some(HomebrewCaskConfig::default()),
        ..Default::default()
    };
    let mut ctx = hb_ctx(cfg, false);
    super::publish_cask(&mut ctx, "mytool", &quiet_log())
        .expect("a tap-less cask must skip, not fail");
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "homebrew-cask");
    assert_eq!(events[0].label, "mytool");
    assert_eq!(events[0].reason, "repository.name is not set");
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
fn publish_top_level_homebrew_casks_missing_repository_skips() {
    let config = Config {
        homebrew_casks: Some(vec![HomebrewCaskConfig {
            name: Some("mycask".to_string()),
            repository: None,
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("a tap-less entry must skip, not fail");
    assert!(!got.pushed_any);
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "homebrew-cask");
    assert_eq!(events[0].label, "mycask");
    assert_eq!(events[0].reason, "repository.name is not set");
}

/// publish_top_level_homebrew_casks: a tap-less entry disqualifies itself
/// only — the entry after it still reaches its publish path.
#[test]
fn publish_top_level_casks_skip_does_not_stop_later_entries() {
    let config = Config {
        homebrew_casks: Some(vec![
            HomebrewCaskConfig {
                name: Some("broken".to_string()),
                repository: None,
                ..Default::default()
            },
            HomebrewCaskConfig {
                name: Some("healthy".to_string()),
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("homebrew-cask-tap".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let (log, capture) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &log)
        .expect("a tap-less entry must not fail the publisher");
    assert_eq!(got.total, 2);

    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].label, "broken");
    let logged: String = capture
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        logged.contains("would update Homebrew cask 'myorg/homebrew-cask-tap'"),
        "the entry after the skipped one must still run; got: {logged}"
    );
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

pub(super) fn art_with_url_sha(
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

/// A context holding `artifacts`, for the cask-download presence tests.
fn ctx_with_artifacts(artifacts: Vec<Artifact>) -> Context {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    for a in artifacts {
        ctx.artifacts.add(a);
    }
    ctx
}

/// A disk image built without a target is a macOS cask download, at the top
/// level and for its crate, as `build_cask_platform_blocks` places it.
#[test]
fn a_targetless_disk_image_counts_as_a_macos_cask_download() {
    let mut dmg = art_with_url_sha(
        ArtifactKind::DiskImage,
        "mytool.dmg",
        "",
        "https://e.com/mytool.dmg",
        "dmgsha",
    );
    dmg.target = None;
    let ctx = ctx_with_artifacts(vec![dmg]);
    assert!(super::cask_scope::has_top_level_cask_artifact(&ctx, None));
    assert!(super::crate_has_macos_cask_artifact(&ctx, "mytool"));
}

/// A darwin `UploadableBinary` alone is a macOS cask download for both
/// presence checks.
#[test]
fn a_lone_darwin_binary_counts_as_a_macos_cask_download() {
    let ctx = ctx_with_artifacts(vec![art_with_url_sha(
        ArtifactKind::UploadableBinary,
        "mytool",
        "aarch64-apple-darwin",
        "https://e.com/mytool",
        "binsha",
    )]);
    assert!(super::cask_scope::has_top_level_cask_artifact(&ctx, None));
    assert!(super::crate_has_macos_cask_artifact(&ctx, "mytool"));
}

/// Linux artifacts, Apple-but-not-macOS archives (`*-apple-ios`, `-watchos`,
/// `-tvos`) and a target-less archive are no macOS cask download.
#[test]
fn linux_and_non_macos_apple_artifacts_are_no_macos_cask_download() {
    let mut targetless = art_with_url_sha(
        ArtifactKind::Archive,
        "mytool.tar.gz",
        "",
        "https://e.com/mytool.tar.gz",
        "sha",
    );
    targetless.target = None;
    let mut artifacts = vec![
        targetless,
        art_with_url_sha(
            ArtifactKind::Archive,
            "mytool-linux.tar.gz",
            "x86_64-unknown-linux-gnu",
            "https://e.com/mytool.tar.gz",
            "linuxsha",
        ),
    ];
    for triple in [
        "aarch64-apple-ios",
        "aarch64-apple-watchos",
        "aarch64-apple-tvos",
    ] {
        artifacts.push(art_with_url_sha(
            ArtifactKind::Archive,
            &format!("mytool-{triple}.tar.gz"),
            triple,
            "https://e.com/mytool.tar.gz",
            "sha",
        ));
    }
    let ctx = ctx_with_artifacts(artifacts);
    assert!(!super::cask_scope::has_top_level_cask_artifact(&ctx, None));
    assert!(!super::crate_has_macos_cask_artifact(&ctx, "mytool"));
}

/// The `ids:` filter narrows the top-level presence check, which is the
/// pair of answers the publisher reads to call an unmatched filter a typo.
#[test]
fn has_top_level_cask_artifact_filters_by_id() {
    let mut a = art_with_url_sha(
        ArtifactKind::UploadableBinary,
        "mytool",
        "aarch64-apple-darwin",
        "https://e.com/mytool",
        "binsha",
    );
    a.metadata.insert("id".to_string(), "stable".to_string());
    let ctx = ctx_with_artifacts(vec![a]);
    assert!(!super::cask_scope::has_top_level_cask_artifact(
        &ctx,
        Some(&["nightly".to_string()])
    ));
    assert!(super::cask_scope::has_top_level_cask_artifact(
        &ctx,
        Some(&["stable".to_string()])
    ));
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
        depends_on_macos: None,
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
        depends_on_macos: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(cask.contains("name \"MyTool\""));
    assert!(cask.contains("name \"alt-1\""));
    assert!(cask.contains("name \"alt-2\""));
}

/// An unset cask `livecheck` emits
/// `livecheck do\n  skip "Auto-generated on release."\nend`: a binary cask's
/// download URL/sha256 are rewritten every release, so there's nothing stable
/// to poll.
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
        depends_on_macos: None,
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
        depends_on_macos: None,
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
        depends_on_macos: None,
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

/// `generate_cask` renders `service` as the cask artifact stanza that names a
/// Services-menu bundle in the download. A `service do … end` block is the
/// formula DSL, and a cask carrying one fails to load.
#[test]
fn generate_cask_emits_service_stanza() {
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
        service: Some("MyTool.workflow"),
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
        depends_on_macos: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(
        cask.contains("\n  service \"MyTool.workflow\"\nend\n"),
        "{cask}"
    );
    assert!(!cask.contains("service do"), "{cask}");
}

/// `generate_cask` emits `depends_on` and `conflicts_with` directives
/// when set.
#[test]
fn generate_cask_emits_depends_and_conflicts() {
    use super::cask::CaskParams;
    let deps = vec!["cask: \"other-app\"".to_string()];
    let cfs = vec!["old-app".to_string()];
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
        depends_on_macos: None,
    };
    let cask = super::cask::generate_cask(&params).unwrap();
    assert!(
        cask.contains(
            "\n  conflicts_with cask: \"old-app\"\n  depends_on cask: \"other-app\"\n  depends_on :macos\nend\n"
        ),
        "conflicts_with precedes depends_on in one group, and a cask with no Linux side depends on macOS:\n{cask}"
    );
}

/// The `depends_on` lines of a rendered cask.
fn depends_on_lines(cask: &str) -> Vec<&str> {
    cask.lines()
        .filter(|l| l.trim_start().starts_with("depends_on"))
        .collect()
}

/// `depends_on :macos` is rendered for every cask with no Linux side:
/// Homebrew's `Cask#supports_linux?` is true for any cask without a macOS
/// dependency, so a binaries-only darwin cask without the line would install
/// a Mach-O binary on Linux. A Linux side (an `on_linux` download, or an
/// `on_linux` / `on_system` block or `app_image` in `custom_block`) or an OS
/// dependency the `custom_block` already declares leaves it out.
#[test]
fn depends_on_macos_is_rendered_for_every_cask_without_a_linux_side() {
    let bins = vec![super::cask::CaskBinaryEntry {
        name: "mytool".to_string(),
        target: None,
    }];
    let linux = |a: &str, c: char| CaskArchEntry {
        arch_block: a.to_string(),
        url: format!("https://e.com/mytool-linux-{a}.tar.gz"),
        sha256: c.to_string().repeat(64),
    };
    let macos_only = ["  depends_on :macos"];

    let mut binary_only = empty_cask_params("mytool", "1.0.0");
    binary_only.binaries = &bins;
    let cask = generate_cask(&binary_only).unwrap();
    assert_eq!(depends_on_lines(&cask), macos_only, "binary-only:\n{cask}");

    let mut app_only = empty_cask_params("mytool", "1.0.0");
    app_only.app = Some("MyTool.app");
    let cask = generate_cask(&app_only).unwrap();
    assert_eq!(depends_on_lines(&cask), macos_only, "app:\n{cask}");

    let mut pkg_in_custom = empty_cask_params("mytool", "1.0.0");
    pkg_in_custom.custom_block = Some("pkg \"MyTool.pkg\"");
    let cask = generate_cask(&pkg_in_custom).unwrap();
    assert_eq!(depends_on_lines(&cask), macos_only, "pkg:\n{cask}");

    let mut app_inside_on_macos = empty_cask_params("mytool", "1.0.0");
    app_inside_on_macos.custom_block = Some("on_macos do\n  app \"Other.app\"\nend");
    let cask = generate_cask(&app_inside_on_macos).unwrap();
    assert_eq!(
        depends_on_lines(&cask),
        macos_only,
        "a stanza inside on_macos:\n{cask}"
    );

    let mut own_dependency = empty_cask_params("mytool", "1.0.0");
    own_dependency.app = Some("MyTool.app");
    own_dependency.custom_block = Some("depends_on macos: :sonoma");
    let cask = generate_cask(&own_dependency).unwrap();
    assert_eq!(
        depends_on_lines(&cask),
        ["  depends_on macos: :sonoma"],
        "custom_block depends_on:\n{cask}"
    );

    let mut linux_only = empty_cask_params("mytool", "1.0.0");
    linux_only.app = Some("MyTool.app");
    linux_only.binaries = &bins;
    linux_only.platforms = vec![CaskPlatformBlock {
        os_block: "linux".to_string(),
        arches: vec![linux("intel", 'c'), linux("arm", 'd')],
    }];
    let cask = generate_cask(&linux_only).unwrap();
    assert!(depends_on_lines(&cask).is_empty(), "linux-only:\n{cask}");

    let mut app_image = empty_cask_params("mytool", "1.0.0");
    app_image.binaries = &bins;
    app_image.custom_block = Some("app_image \"MyTool.AppImage\"");
    let cask = generate_cask(&app_image).unwrap();
    assert!(depends_on_lines(&cask).is_empty(), "app_image:\n{cask}");
}

/// A flat single-download cask whose `custom_block` declares a Linux side
/// (`on_linux` or `on_system`) is cross-platform: the flat `sha256` / `url`
/// and the macOS-only `app` / `service` move into `on_macos`, so a Linux host
/// never downloads the macOS archive, and no `depends_on :macos` is added.
#[test]
fn a_flat_cask_beside_a_custom_linux_block_puts_its_download_inside_on_macos() {
    let bins = vec![super::cask::CaskBinaryEntry {
        name: "mytool".to_string(),
        target: None,
    }];
    for custom in [
        "on_linux do\n  binary \"mytool-linux\"\nend",
        "on_system :linux, macos: :sonoma_or_newer do\n  binary \"mytool-linux\"\nend",
    ] {
        let mut params = empty_cask_params("mytool", "1.0.0");
        params.sha256 = "deadbeef";
        params.url = "https://e.com/v#{version}/mytool-darwin-universal.tar.gz";
        params.app = Some("MyTool.app");
        params.service = Some("MyTool.workflow");
        params.binaries = &bins;
        params.custom_block = Some(custom);
        let cask = generate_cask(&params).unwrap();
        assert!(
            cask.contains(
                "  version \"1.0.0\"\n\n  on_macos do\n    sha256 \"deadbeef\"\n\n    url \"https://e.com/v#{version}/mytool-darwin-universal.tar.gz\"\n\n    app \"MyTool.app\"\n    service \"MyTool.workflow\"\n  end\n\n  name \"mytool\"\n"
            ),
            "{custom}:\n{cask}"
        );
        assert_eq!(
            cask.matches("sha256").count(),
            1,
            "the flat sha256 is written once:\n{cask}"
        );
        assert!(depends_on_lines(&cask).is_empty(), "{custom}:\n{cask}");
        assert!(
            cask.contains("\n  binary \"mytool\"\n"),
            "binary stays top-level:\n{cask}"
        );
    }
}

/// `depends_on_macos:` decides both halves of the scan's verdict: `true`
/// renders `depends_on :macos` beside a flat top-level download, `false`
/// renders no dependency and moves a flat download into `on_macos`, and an
/// unset field keeps what the scan decided.
#[test]
fn depends_on_macos_overrides_the_custom_block_scan() {
    let flat_top_level =
        "  version \"1.0.0\"\n  sha256 \"deadbeef\"\n\n  url \"https://e.com/x.tar.gz\"\n";
    let flat_in_on_macos = "  version \"1.0.0\"\n\n  on_macos do\n    sha256 \"deadbeef\"\n\n    url \"https://e.com/x.tar.gz\"\n  end\n";
    // (custom_block, override, depends_on :macos rendered, download layout)
    let cases: &[(Option<&str>, Option<bool>, bool, &str)] = &[
        (None, None, true, flat_top_level),
        (None, Some(false), false, flat_in_on_macos),
        (None, Some(true), true, flat_top_level),
        // The scan sets a regex after `when` aside, so its `on_linux` is
        // no Linux side.
        (
            Some("case x\nwhen /on_linux/ then 1\nend"),
            None,
            true,
            flat_top_level,
        ),
        (
            Some("on_linux do\n  binary \"x\"\nend"),
            Some(true),
            true,
            flat_top_level,
        ),
        (
            Some("on_linux do\n  binary \"x\"\nend"),
            None,
            false,
            flat_in_on_macos,
        ),
        (
            Some("on_linux do\n  binary \"x\"\nend"),
            Some(false),
            false,
            flat_in_on_macos,
        ),
    ];
    for (custom, over, macos, layout) in cases {
        let mut params = empty_cask_params("mytool", "1.0.0");
        params.url = "https://e.com/x.tar.gz";
        params.custom_block = *custom;
        params.depends_on_macos = *over;
        let cask = generate_cask(&params).unwrap();
        let want: &[&str] = if *macos {
            &["  depends_on :macos"]
        } else {
            &[]
        };
        assert_eq!(
            depends_on_lines(&cask),
            want,
            "{custom:?} {over:?}:\n{cask}"
        );
        assert!(cask.contains(layout), "{custom:?} {over:?}:\n{cask}");
    }

    // Beside per-arch macOS downloads, `false` drops the dependency and
    // leaves the blocks as they were.
    let mut params = empty_cask_params("mytool", "1.0.0");
    params.platforms = vec![CaskPlatformBlock {
        os_block: "macos".to_string(),
        arches: vec![CaskArchEntry {
            arch_block: "arm".to_string(),
            url: "https://e.com/arm.tar.gz".to_string(),
            sha256: "aa".to_string(),
        }],
    }];
    let with_scan = generate_cask(&params).unwrap();
    params.depends_on_macos = Some(false);
    let overridden = generate_cask(&params).unwrap();
    assert_eq!(depends_on_lines(&with_scan), ["  depends_on :macos"]);
    assert!(depends_on_lines(&overridden).is_empty(), "{overridden}");
    assert_eq!(
        overridden,
        with_scan.replace("\n\n  depends_on :macos", ""),
        "only the dependency differs"
    );
}

/// A `universal_binaries` (`replace: true`) macOS artifact beside Linux
/// per-arch artifacts renders as a flat `sha256`/`url` inside `on_macos`,
/// with `on_linux` keeping its two arch blocks.
#[test]
fn a_universal_macos_artifact_beside_linux_renders_flat_inside_on_macos() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let mut ctx = top_cask_ctx(&cask_cfg);
    ctx.artifacts = Default::default();
    for (name, target, sha) in [
        ("mytool-darwin-universal.tar.gz", "darwin-universal", "a"),
        ("mytool-linux-amd64.tar.gz", "x86_64-unknown-linux-gnu", "c"),
        (
            "mytool-linux-arm64.tar.gz",
            "aarch64-unknown-linux-gnu",
            "d",
        ),
    ] {
        ctx.artifacts.add(art_with_url_sha(
            ArtifactKind::Archive,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &sha.repeat(64),
        ));
    }
    let c = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable")
        .content;
    let expected = format!(
        "  on_macos do\n    sha256 \"{a}\"\n\n    url \"https://e.com/v#{{version}}/mytool-darwin-universal.tar.gz\"\n  end\n  on_linux do\n    on_arm do\n      sha256 \"{d}\"\n      url \"https://e.com/v#{{version}}/mytool-linux-arm64.tar.gz\"\n    end\n    on_intel do\n      sha256 \"{c}\"\n      url \"https://e.com/v#{{version}}/mytool-linux-amd64.tar.gz\"\n    end\n  end\n",
        a = "a".repeat(64),
        c = "c".repeat(64),
        d = "d".repeat(64),
    );
    assert!(c.contains(&expected), "{c}");
}

/// A universal macOS artifact beside per-arch macOS ones for both
/// architectures is left out and logged at verbose, and the per-arch blocks
/// render as they would without it.
#[test]
fn a_universal_artifact_beside_both_per_arch_ones_is_dropped_and_logged() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    // `top_cask_ctx` already holds the darwin amd64 archive.
    let mut ctx = top_cask_ctx(&cask_cfg);
    for (name, target) in [
        ("mytool-darwin-universal.tar.gz", "darwin-universal"),
        ("mytool-darwin-arm64.tar.gz", "aarch64-apple-darwin"),
    ] {
        ctx.artifacts.add(art_with_url_sha(
            ArtifactKind::Archive,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &"b".repeat(64),
        ));
    }
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let c = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &log)
        .expect("top-level cask render")
        .expect("top-level cask applicable")
        .content;
    assert!(
        c.contains("    on_arm do\n") && c.contains("    on_intel do\n"),
        "{c}"
    );
    assert!(!c.contains("universal"), "{c}");
    let verbose: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(level, _)| *level == anodizer_core::log::LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        verbose,
        [
            "skipped universal artifact 'mytool-darwin-universal.tar.gz' — both macOS architectures have their own download"
        ],
        "{c}"
    );
}

/// A universal macOS artifact beside a per-arch one for one architecture
/// fills the other architecture's slot: `on_arm` downloads the arm64 build
/// and `on_intel` the universal one.
#[test]
fn a_universal_artifact_fills_the_architecture_no_per_arch_build_covers() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let mut ctx = top_cask_ctx(&cask_cfg);
    ctx.artifacts = Default::default();
    for (name, target, sha) in [
        ("mytool-darwin-universal.tar.gz", "darwin-universal", 'a'),
        ("mytool-darwin-arm64.tar.gz", "aarch64-apple-darwin", 'b'),
    ] {
        let mut art = art_with_url_sha(
            ArtifactKind::Archive,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &sha.to_string().repeat(64),
        );
        art.metadata.insert("id".to_string(), "cli".to_string());
        ctx.artifacts.add(art);
    }
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let c = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &log)
        .expect("top-level cask render")
        .expect("top-level cask applicable")
        .content;
    let expected = format!(
        "  on_macos do\n    on_arm do\n      sha256 \"{b}\"\n      url \"https://e.com/v#{{version}}/mytool-darwin-arm64.tar.gz\"\n    end\n    on_intel do\n      sha256 \"{a}\"\n      url \"https://e.com/v#{{version}}/mytool-darwin-universal.tar.gz\"\n    end\n  end\n",
        a = "a".repeat(64),
        b = "b".repeat(64),
    );
    assert!(c.contains(&expected), "{c}");
    assert_eq!(cap.verbose_count(), 0, "nothing was skipped");
}

/// An `Archive` carrying `url`, `sha256` and a build `id`.
fn art_with_id(kind: ArtifactKind, name: &str, target: &str, id: &str) -> Artifact {
    let mut art = art_with_url_sha(
        kind,
        name,
        target,
        &format!("https://e.com/v1.2.3/{name}"),
        &"c".repeat(64),
    );
    art.metadata.insert("id".to_string(), id.to_string());
    art
}

/// A universal macOS artifact fills the empty slot only beside a per-arch
/// artifact of the same build id. Of another build it is left out with a
/// verbose line naming both ids, and the one per-arch download renders flat.
#[test]
fn a_universal_artifact_fills_a_slot_only_beside_its_own_build() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    for (universal_id, filled) in [("cli", true), ("gui", false)] {
        let mut ctx = top_cask_ctx(&cask_cfg);
        ctx.artifacts = Default::default();
        ctx.artifacts.add(art_with_id(
            ArtifactKind::Archive,
            "mytool-darwin-universal.tar.gz",
            "darwin-universal",
            universal_id,
        ));
        ctx.artifacts.add(art_with_id(
            ArtifactKind::Archive,
            "mytool-darwin-arm64.tar.gz",
            "aarch64-apple-darwin",
            "cli",
        ));
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let c = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &log)
            .expect("top-level cask render")
            .expect("top-level cask applicable")
            .content;
        let verbose: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter(|(level, _)| *level == anodizer_core::log::LogLevel::Verbose)
            .map(|(_, m)| m)
            .collect();
        if filled {
            assert!(c.contains("    on_intel do\n"), "{c}");
            assert!(c.contains("mytool-darwin-universal.tar.gz"), "{c}");
            assert!(verbose.is_empty(), "{verbose:?}");
        } else {
            assert!(!c.contains("on_intel"), "{c}");
            assert!(!c.contains("universal"), "{c}");
            assert!(
                c.contains("\n  url \"https://e.com/v#{version}/mytool-darwin-arm64.tar.gz\""),
                "{c}"
            );
            assert_eq!(
                verbose,
                [
                    "skipped universal artifact 'mytool-darwin-universal.tar.gz' — build id 'gui' does not match 'cli', the build of the macOS arm download"
                ],
                "{c}"
            );
        }
    }
}

/// A universal macOS artifact never fills the empty slot when either side
/// has no build id: two artifacts without an id are not known to be one
/// build. Each side's missing id is named in its own verbose line.
#[test]
fn a_universal_artifact_without_a_matching_id_on_both_sides_fills_no_slot() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let cases = [
        (
            None,
            Some("cli"),
            "skipped universal artifact 'mytool-darwin-universal.tar.gz' — it has no id to match against the macOS arm download",
        ),
        (
            Some("cli"),
            None,
            "skipped universal artifact 'mytool-darwin-universal.tar.gz' — the macOS arm download has no id",
        ),
        (
            None,
            None,
            "skipped universal artifact 'mytool-darwin-universal.tar.gz' — it has no id to match against the macOS arm download",
        ),
    ];
    for (universal_id, arm_id, line) in cases {
        let mut ctx = top_cask_ctx(&cask_cfg);
        ctx.artifacts = Default::default();
        for (name, target, id) in [
            (
                "mytool-darwin-universal.tar.gz",
                "darwin-universal",
                universal_id,
            ),
            ("mytool-darwin-arm64.tar.gz", "aarch64-apple-darwin", arm_id),
        ] {
            let mut art = art_with_url_sha(
                ArtifactKind::Archive,
                name,
                target,
                &format!("https://e.com/v1.2.3/{name}"),
                &"c".repeat(64),
            );
            if let Some(id) = id {
                art.metadata.insert("id".to_string(), id.to_string());
            }
            ctx.artifacts.add(art);
        }
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let c = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &log)
            .expect("top-level cask render")
            .expect("top-level cask applicable")
            .content;
        assert!(!c.contains("universal"), "{c}");
        let verbose: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter(|(level, _)| *level == anodizer_core::log::LogLevel::Verbose)
            .map(|(_, m)| m)
            .collect();
        assert_eq!(verbose, [line], "{universal_id:?} / {arm_id:?}");
    }
}

/// A `-v` context with a log capture, holding a top-level cask whose
/// universal artifact sits beside per-arch builds for both architectures.
fn universal_beside_both_arches_ctx() -> (
    Context,
    anodizer_core::config::HomebrewCaskConfig,
    anodizer_core::log::LogCapture,
) {
    let cask_cfg = anodizer_core::config::HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let mut ctx = top_cask_ctx(&cask_cfg);
    ctx.artifacts = Default::default();
    for (name, target) in [
        ("mytool-darwin-universal.tar.gz", "darwin-universal"),
        ("mytool-darwin-arm64.tar.gz", "aarch64-apple-darwin"),
        ("mytool-darwin-amd64.tar.gz", "x86_64-apple-darwin"),
    ] {
        ctx.artifacts.add(art_with_url_sha(
            ArtifactKind::Archive,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &"b".repeat(64),
        ));
    }
    ctx.options.verbose = true;
    let cap = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(cap.clone());
    (ctx, cask_cfg, cap)
}

const BOTH_ARCHES_SKIP_LINE: &str = "skipped universal artifact 'mytool-darwin-universal.tar.gz' — both macOS architectures have their own download";

/// One of the passes that render a homebrew manifest in a run.
#[derive(Clone, Copy, Debug)]
pub(super) enum RenderPass {
    /// The `emission-validate` stage.
    Emission,
    /// The pre-publish guard, which renders strict.
    Guard,
    /// The publisher itself.
    Publisher,
}

/// The passes each command runs, in pipeline order, and whether it runs
/// them under `--dry-run`. A release and a `--merge` run all three;
/// `--publish-only`, `publish` and `continue` have no emission validation;
/// a dry run keeps every pass and the publishers stop before their push;
/// `--snapshot` skips the guard and the publish stage, and `--nightly`
/// skips the guard and the homebrew publisher (`skips_on_nightly`).
pub(super) const COMMAND_PASSES: &[(&str, bool, &[RenderPass])] = &[
    (
        "release",
        false,
        &[
            RenderPass::Emission,
            RenderPass::Guard,
            RenderPass::Publisher,
        ],
    ),
    (
        "release --dry-run",
        true,
        &[
            RenderPass::Emission,
            RenderPass::Guard,
            RenderPass::Publisher,
        ],
    ),
    (
        "release --publish-only / publish / continue",
        false,
        &[RenderPass::Guard, RenderPass::Publisher],
    ),
    (
        "release --publish-only / publish / continue, each with --dry-run",
        true,
        &[RenderPass::Guard, RenderPass::Publisher],
    ),
    (
        "release --merge",
        false,
        &[
            RenderPass::Emission,
            RenderPass::Guard,
            RenderPass::Publisher,
        ],
    ),
    ("release --snapshot", false, &[RenderPass::Emission]),
    ("release --nightly", false, &[RenderPass::Emission]),
];

/// Run `pass` the way its stage does: the guard flips strict rendering on
/// around the schema pass, and the publisher runs `publish`.
pub(super) fn run_render_pass(
    ctx: &mut Context,
    pass: RenderPass,
    publish: &dyn Fn(&mut Context, &StageLogger),
) {
    use crate::schema_validation::{test_current_version_resolver, validate_publisher_schemas};
    let log = ctx.logger("publish");
    match pass {
        RenderPass::Emission => {
            validate_publisher_schemas(ctx, &log, &test_current_version_resolver())
                .expect("emission validation passes");
        }
        RenderPass::Guard => {
            let was = ctx.set_render_strict(true);
            validate_publisher_schemas(ctx, &log, &test_current_version_resolver())
                .expect("the guard passes");
            ctx.set_render_strict(was);
        }
        RenderPass::Publisher => publish(ctx, &log),
    }
}

/// Under `-v`, every command prints the universal skip line of a top-level
/// cask once, whichever of its passes renders the cask first.
#[test]
fn the_universal_skip_line_prints_once_per_command() {
    for (command, dry_run, passes) in COMMAND_PASSES {
        let (bare_url, bare) = make_bare_cask_tap("main");
        let (mut ctx, _cask_cfg, cap) = universal_beside_both_arches_ctx();
        ctx.options.dry_run = *dry_run;
        ctx.config.homebrew_casks.as_mut().unwrap()[0].repository =
            local_cask_cfg(&bare_url, "main").repository;
        ctx.template_vars_mut().set("RawVersion", "1.2.3");
        for pass in *passes {
            run_render_pass(&mut ctx, *pass, &|ctx, log| {
                let got =
                    super::publish_top_level_homebrew_casks(ctx, log).expect("the cask publishes");
                assert_eq!(got.pushed_any, !ctx.is_dry_run(), "{got:?}");
            });
        }
        let printed: Vec<String> = cap
            .printed_messages(anodizer_core::log::LogLevel::Verbose)
            .into_iter()
            .filter(|m| m.starts_with("skipped universal artifact"))
            .collect();
        assert_eq!(printed, [BOTH_ARCHES_SKIP_LINE], "{command}");
        drop(bare);
    }
}

/// A warning the formula render emits (an archive dropped for a duplicate
/// platform) prints once per command, and so does the same-tap cask's
/// universal skip line beside it.
#[test]
fn a_formula_render_warning_prints_once_per_command() {
    let dropped = "homebrew: crate 'mytool': ";
    for (command, dry_run, passes) in COMMAND_PASSES {
        let (bare_url, bare) = make_bare_cask_tap("main");
        let mut hb = HomebrewConfig {
            repository: local_cask_cfg(&bare_url, "main").repository,
            cask: Some(HomebrewCaskConfig {
                name: Some("mytool".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        hb.repository.as_mut().unwrap().name = Some("homebrew-tap".to_string());
        let config = Config {
            project_name: "mytool".to_string(),
            crates: vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig {
                    homebrew: Some(hb),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                verbose: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("RawVersion", "1.2.3");
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        for (name, target, format) in [
            (
                "mytool-darwin-universal.tar.gz",
                "darwin-universal",
                "tar.gz",
            ),
            (
                "mytool-darwin-arm64.tar.gz",
                "aarch64-apple-darwin",
                "tar.gz",
            ),
            (
                "mytool-darwin-amd64.tar.gz",
                "x86_64-apple-darwin",
                "tar.gz",
            ),
            (
                "mytool-linux-amd64.tar.gz",
                "x86_64-unknown-linux-gnu",
                "tar.gz",
            ),
            ("mytool-linux-amd64.zip", "x86_64-unknown-linux-gnu", "zip"),
        ] {
            let mut art = art_with_url_sha(
                ArtifactKind::Archive,
                name,
                target,
                &format!("https://e.com/v1.2.3/{name}"),
                &"c".repeat(64),
            );
            art.metadata
                .insert("format".to_string(), format.to_string());
            ctx.artifacts.add(art);
        }
        ctx.options.dry_run = *dry_run;
        let cap = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(cap.clone());
        for pass in *passes {
            run_render_pass(&mut ctx, *pass, &|ctx, log| {
                assert_eq!(
                    super::publish_to_homebrew(ctx, "mytool", log).expect("the formula publishes"),
                    !ctx.is_dry_run()
                );
            });
        }
        let warned: Vec<String> = cap
            .printed_messages(anodizer_core::log::LogLevel::Warn)
            .into_iter()
            .filter(|m| m.starts_with(dropped))
            .collect();
        assert_eq!(warned.len(), 1, "{command}: {warned:?}");
        assert!(
            warned[0].contains("dropped 'https://e.com/v1.2.3/mytool-linux-amd64.zip'"),
            "{command}: {warned:?}"
        );
        let skipped: Vec<String> = cap
            .printed_messages(anodizer_core::log::LogLevel::Verbose)
            .into_iter()
            .filter(|m| m.starts_with("skipped universal artifact"))
            .collect();
        assert_eq!(skipped, [BOTH_ARCHES_SKIP_LINE], "{command}");
        drop(bare);
    }
}

/// A universal disk image beside per-arch `UploadableBinary` artifacts for
/// both architectures is left out, and the per-arch blocks render from the
/// binaries, in the per-crate cask and the top-level one alike.
#[test]
fn a_universal_dmg_beside_per_arch_binaries_renders_the_per_arch_blocks() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig};
    let cask_cfg = HomebrewCaskConfig {
        name: Some("mytool".to_string()),
        ..Default::default()
    };
    let mut ctx = top_cask_ctx(&cask_cfg);
    ctx.artifacts = Default::default();
    for (kind, name, target) in [
        (
            ArtifactKind::DiskImage,
            "mytool-darwin-universal.dmg",
            "darwin-universal",
        ),
        (
            ArtifactKind::UploadableBinary,
            "mytool-darwin-arm64",
            "aarch64-apple-darwin",
        ),
        (
            ArtifactKind::UploadableBinary,
            "mytool-darwin-amd64",
            "x86_64-apple-darwin",
        ),
    ] {
        ctx.artifacts.add(art_with_url_sha(
            kind,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &"d".repeat(64),
        ));
    }
    let per_crate = super::cask_scope::generate_cask_from_context(
        &ctx,
        "mytool",
        &HomebrewConfig::default(),
        &cask_cfg,
        &test_log(),
    )
    .expect("per-crate cask render")
    .content;
    let top = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
        .expect("top-level cask render")
        .expect("top-level cask applicable")
        .content;
    for c in [per_crate, top] {
        assert!(
            c.contains("url \"https://e.com/v#{version}/mytool-darwin-arm64\"")
                && c.contains("url \"https://e.com/v#{version}/mytool-darwin-amd64\""),
            "{c}"
        );
        assert!(!c.contains(".dmg"), "{c}");
    }
}

/// The skip line `publish/homebrew-casks.md` quotes is the one the renderer
/// logs for a universal artifact beside both per-arch ones.
#[test]
fn the_universal_skip_line_quoted_in_the_cask_docs_is_what_the_renderer_logs() {
    use anodizer_core::config::HomebrewCaskConfig;
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/publish/homebrew-casks.md"
    ))
    .expect("read the cask docs page");
    let quoted: Vec<&str> = page
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("skipped universal artifact"))
        .collect();
    assert_eq!(quoted.len(), 1, "{quoted:?}");

    let cask_cfg = HomebrewCaskConfig {
        name: Some("myapp".to_string()),
        ..Default::default()
    };
    let mut ctx = top_cask_ctx(&cask_cfg);
    ctx.artifacts = Default::default();
    for (name, target) in [
        ("myapp-darwin-universal.tar.gz", "darwin-universal"),
        ("myapp-darwin-arm64.tar.gz", "aarch64-apple-darwin"),
        ("myapp-darwin-amd64.tar.gz", "x86_64-apple-darwin"),
    ] {
        ctx.artifacts.add(art_with_url_sha(
            ArtifactKind::Archive,
            name,
            target,
            &format!("https://e.com/v1.2.3/{name}"),
            &"e".repeat(64),
        ));
    }
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &log)
        .expect("top-level cask render")
        .expect("top-level cask applicable");
    let produced: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(level, _)| *level == anodizer_core::log::LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(produced, quoted);
}

/// The universal `url` written directly inside `on_macos` carries every
/// `url:` option, with each continuation line two columns left of where the
/// nested per-arch `url` would put it: the arguments line up under the first
/// one, and the multi-line array and hash keep their shape. `verified:` is
/// never written.
#[test]
fn a_universal_url_inside_on_macos_lays_out_its_options_byte_for_byte() {
    use super::cask::{ARCH_URL_ARGS_INDENT, render_additional_url_params};
    let url_cfg = HomebrewCaskURL {
        verified: Some("e.com/".to_string()),
        using: Some(":homebrew_curl".to_string()),
        cookies: Some(HashMap::from([
            ("session".to_string(), "abc".to_string()),
            ("license".to_string(), "accepted".to_string()),
        ])),
        headers: Some(vec![
            "Accept: application/octet-stream".to_string(),
            "X-Client: anodizer".to_string(),
        ]),
        user_agent: Some("anodizer/1.0".to_string()),
        ..Default::default()
    };
    let url_extras_arch = render_additional_url_params(&url_cfg, ARCH_URL_ARGS_INDENT);
    let mut params = empty_cask_params("mytool", "1.0.0");
    params.url_extras_indented = &url_extras_arch;
    params.platforms = vec![CaskPlatformBlock {
        os_block: "macos".to_string(),
        arches: vec![CaskArchEntry {
            arch_block: super::cask::UNIVERSAL_ARCH_BLOCK.to_string(),
            url: "https://e.com/v#{version}/mytool-darwin-universal.tar.gz".to_string(),
            sha256: "a".repeat(64),
        }],
    }];
    let cask = generate_cask(&params).unwrap();
    let expected = r##"  on_macos do
    sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

    url "https://e.com/v#{version}/mytool-darwin-universal.tar.gz",
        using:      :homebrew_curl,
        cookies:    {
          "license" => "accepted",
          "session" => "abc",
        },
        header:     [
          "Accept: application/octet-stream",
          "X-Client: anodizer",
        ],
        user_agent: "anodizer/1.0"
  end
"##;
    assert!(cask.contains(expected), "{cask}");
    assert!(!cask.contains("verified"), "{cask}");
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
        depends_on_macos: None,
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
/// followed by a missing-repository entry continues past both and records the
/// second entry's skip against its own cask name, not the first-by-index.
#[test]
fn publish_top_level_homebrew_casks_skip_then_missing_repository_records_both() {
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
    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("a tap-less second entry must skip, not fail the run");
    assert!(!got.pushed_any);
    let repo_skips: Vec<_> = ctx
        .skip_memento
        .snapshot()
        .into_iter()
        .filter(|e| e.reason == "repository.name is not set")
        .collect();
    assert_eq!(repo_skips.len(), 1, "{:?}", ctx.skip_memento.snapshot());
    assert_eq!(repo_skips[0].label, "broken-cask");
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
pub(super) fn make_bare_cask_tap(branch: &str) -> (String, tempfile::TempDir) {
    let bare = tempfile::tempdir().expect("bare tempdir");
    let seed = tempfile::tempdir().expect("seed tempdir");
    let git_ok =
        |dir: &std::path::Path, args: &[&str]| anodizer_core::test_helpers::git_test_ok(dir, args);
    git_ok(bare.path(), &["init", "--bare", "-b", branch]);
    git_ok(seed.path(), &["init", "-b", branch]);
    git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
    git_ok(seed.path(), &["config", "user.name", "T"]);
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

    // First publish puts the cask on `main`.
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
/// update_existing_pr eval + submit + record_publisher_outcome path.
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
    // The PR-submission path ran: with `gh` absent and no token the transport
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
        depends_on_macos: None,
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

/// `render_additional_url_params` escapes user values spliced into `referer`,
/// `user_agent`, header, cookies, and data string literals; the whole
/// `url "…"` continuation renders to valid Ruby. The `using:` symbol is raw
/// Ruby and stays unescaped.
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
        extras.contains(r#"referer:    "https://r\"ef\\er""#),
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
        extras.contains("using:      :homebrew_curl"),
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
        directive.contains(r#"base_name:              "my\"app\\x""#),
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

/// `build_depends_directives` escapes the package-name string literals inside
/// `cask: "…"` / `formula: "…"`, the cask renderer escapes the conflicting
/// cask names, and the resulting cask is valid Ruby. A `formula:` conflict is
/// left out, because Homebrew's cask `conflicts_with` accepts only `cask:`.
#[test]
fn depends_conflicts_ruby_escape_passes_ruby_c() {
    use anodizer_core::config::{HomebrewCaskConflictEntry, HomebrewCaskDependencyEntry};

    let deps = vec![HomebrewCaskDependencyEntry {
        cask: Some(r#"oth"er\app"#.to_string()),
        formula: None,
    }];
    let conflicts = vec![
        HomebrewCaskConflictEntry {
            cask: Some(r#"old"c\k"#.to_string()),
            formula: None,
        },
        HomebrewCaskConflictEntry {
            cask: None,
            formula: Some("old-formula".to_string()),
        },
    ];
    let dep_dirs = super::formula::build_depends_directives(Some(&deps));
    let conflicting = super::cask::conflicting_casks(Some(&conflicts));

    assert_eq!(dep_dirs, vec![r#"cask: "oth\"er\\app""#.to_string()]);
    assert_eq!(conflicting, vec![r#"old"c\k"#.to_string()]);

    let mut params = empty_cask_params("x", "1.0");
    params.depends_on = &dep_dirs;
    params.conflicts_with = &conflicting;
    let cask = generate_cask(&params).unwrap();
    assert!(
        cask.contains(r#"  conflicts_with cask: "old\"c\\k""#),
        "conflict name must be escaped once:\n{cask}"
    );
    assert!(!cask.contains("old-formula"), "{cask}");
    assert_ruby_syntax_ok("depends_conflicts", &cask);
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
/// prove the escaping does real work: the un-escaped equivalent must be
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
/// This proves the escaping does real work rather than being a no-op the suite
/// would pass even if the filter were deleted.
#[test]
fn ruby_escape_does_real_work() {
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
/// un-escaped name produces invalid Ruby — so the escaping does real work.
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
    // rejected by `ruby -c`, proving the escaping above does real work.
    let naive_line = format!("bin.install \"{name}\" => \"{bin}\"");
    let broken = format!("class T < Formula\n  def install\n    {naive_line}\n  end\nend\n");
    assert_ruby_syntax_err("install-fragment-unescaped", &broken);
}

// -----------------------------------------------------------------------
// Formula renderer: previously-uncovered archive-layout and opts branches.
// -----------------------------------------------------------------------

/// A multi-archive macOS set with only an amd64 entry lists that archive
/// under both `on_arm` and `on_intel` (Homebrew allows a `url` inside
/// `on_macos` only under an architecture block, and an Apple Silicon Mac
/// runs the Intel build), and the top-level `def caveats` warns on Apple
/// Silicon only.
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
    assert!(
        formula.contains(
            r#"  on_macos do
    on_arm do
      url "https://example.com/monotool-darwin-amd64.tar.gz"
      sha256 "machash"
    end
    on_intel do
      url "https://example.com/monotool-darwin-amd64.tar.gz"
      sha256 "machash"
    end
  end
"#
        ),
        "{formula}"
    );
    assert!(
        formula.contains(
            r#"  def caveats
    return if OS.linux? || Hardware::CPU.intel?

    <<~EOS
      The darwin_arm64 architecture is not supported for the monotool
      formula at this time. The darwin_amd64 binary may work in compatibility
      mode, but it might not be fully supported.
    EOS
  end
"#
        ),
        "{formula}"
    );
}

/// The Apple Silicon caveat follows configured caveats, shown on Apple
/// Silicon only; the configured ones show everywhere.
#[test]
fn the_apple_silicon_caveat_follows_the_configured_caveats() {
    let opts = FormulaOptions {
        caveats: Some("Run `monotool init` once.\nThen run `monotool serve`."),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "monotool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            ("darwin-amd64", "https://example.com/mac.tar.gz", "machash"),
            (
                "linux-amd64",
                "https://example.com/linux.tar.gz",
                "linuxhash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"monotool\"",
            test: "system \"#{bin}/monotool\"",
        },
        &opts,
        &test_log(),
    )
    .unwrap();
    assert!(
        formula.contains(
            r#"  def caveats
    message = <<~EOS
      Run `monotool init` once.
      Then run `monotool serve`.
    EOS
    return message if OS.linux? || Hardware::CPU.intel?

    message + <<~EOS
      The darwin_arm64 architecture is not supported for the monotool
"#
        ),
        "{formula}"
    );
}

/// A universal macOS archive serves both architectures, so it is listed
/// under `on_arm` and `on_intel` and carries no Apple Silicon caveat.
#[test]
fn a_universal_macos_archive_is_listed_for_both_architectures_without_a_caveat() {
    let formula = generate_formula(
        &super::formula::FormulaCore {
            name: "unitool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            (
                "darwin-universal",
                "https://example.com/uni.tar.gz",
                "unihash",
            ),
            (
                "linux-amd64",
                "https://example.com/linux.tar.gz",
                "linuxhash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"unitool\"",
            test: "system \"#{bin}/unitool\"",
        },
    )
    .unwrap();
    assert_eq!(
        formula.matches("sha256 \"unihash\"").count(),
        2,
        "{formula}"
    );
    assert!(formula.contains("    on_arm do\n      url \"https://example.com/uni.tar.gz\""));
    assert!(formula.contains("    on_intel do\n      url \"https://example.com/uni.tar.gz\""));
    assert!(!formula.contains("def caveats"), "{formula}");
}

/// Homebrew on Linux runs on x86_64 and arm64 only, so a 32-bit ARM or x86
/// archive is left out with a `-v` line naming it, and the x86_64 archive
/// beside it serves Linux alone, requiring that CPU.
#[test]
fn test_formula_linux_32_bit_archives_are_left_out() {
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Verbose);
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "pitool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[
            (
                "armv7-unknown-linux-gnueabihf",
                "https://example.com/pitool-linux-armv7.tar.gz",
                "armv7hash",
            ),
            (
                "i686-unknown-linux-gnu",
                "https://example.com/pitool-linux-i686.tar.gz",
                "i686hash",
            ),
            (
                "x86_64-unknown-linux-gnu",
                "https://example.com/pitool-linux-amd64.tar.gz",
                "amd64hash",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"pitool\"",
            test: "system \"#{bin}/pitool\"",
        },
        &FormulaOptions::default(),
        &log,
    )
    .unwrap();
    assert!(!formula.contains("armv7hash"), "{formula}");
    assert!(!formula.contains("i686hash"), "{formula}");
    assert_eq!(formula.matches("  url ").count(), 1, "{formula}");
    assert!(formula.contains("  sha256 \"amd64hash\"\n"), "{formula}");
    assert!(
        formula.contains("  depends_on arch: :x86_64\n  depends_on :linux\n"),
        "{formula}"
    );
    assert!(!formula.contains("Hardware::CPU"), "{formula}");
    let verbose: Vec<String> = capture
        .all_messages()
        .into_iter()
        .filter(|(level, _)| *level == anodizer_core::log::LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        verbose,
        [
            "skipped archive 'pitool-linux-armv7.tar.gz' — Homebrew on Linux has no armv7 variant",
            "skipped archive 'pitool-linux-i686.tar.gz' — Homebrew on Linux has no 386 variant",
        ]
    );
}

/// An archive for an OS Homebrew does not run on is left out with a `-v`
/// line; the Linux archive beside it is the whole formula.
#[test]
fn test_formula_unknown_platform_is_left_out() {
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Verbose);
    let formula = generate_formula_with_opts(
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
        &FormulaOptions::default(),
        &log,
    )
    .unwrap();
    assert!(!formula.contains("freebsd"), "{formula}");
    assert!(!formula.contains("bsdhash"), "{formula}");
    assert!(
        formula.contains(
            "  url \"https://example.com/xtool-linux-amd64.tar.gz\"\n  version \"1.0.0\"\n  sha256 \"linuxhash\"\n"
        ),
        "{formula}"
    );
    assert_eq!(
        capture.all_messages(),
        [(
            anodizer_core::log::LogLevel::Verbose,
            "skipped archive 'xtool-freebsd-amd64.tar.gz' — Homebrew has no freebsd variant"
                .to_string()
        )]
    );
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
        &test_log(),
    )
    .unwrap();
    assert!(formula.contains("require_relative \"private_strategy\""));
    assert!(
        formula.contains(
            r#"  url "https://example.com/privtool.tar.gz",
      using:   GitHubPrivateRepositoryReleaseDownloadStrategy,
      headers: [
        "Authorization: Bearer tok",
        "X-Foo: bar",
      ]
  version "2.0.0"
"#
        ),
        "the url arguments must line up after the longest key\n{formula}"
    );
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
        &test_log(),
    )
    .unwrap();
    assert!(formula.contains("chmod 0755, bin/\"extra\""));
    assert!(formula.contains("def post_install"));
    assert!(formula.contains("system \"#{bin}/setup\""));
    assert!(formula.contains("# custom ruby here"));
    // Homebrew 7 has no `plist_options`; a formula naming it fails to load.
    assert!(!formula.contains("plist_options"), "{formula}");
    assert!(formula.contains("def plist"));
    assert!(formula.contains("<plist>stub</plist>"));
    assert!(formula.contains("service do"));
    assert!(formula.contains("run [opt_bin/\"svc\"]"));
}

/// A `plist` is written as `def plist` alone, the form Homebrew 7 still
/// loads, and the render warns that `service` replaces it.
#[test]
fn test_formula_plist_warns_that_service_replaces_it() {
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);
    let opts = FormulaOptions {
        plist: Some("<plist>stub</plist>"),
        ..Default::default()
    };
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: "svctool",
            version: "1.0.0",
            description: "desc",
            license: "MIT",
        },
        &[(
            "aarch64-apple-darwin",
            "https://example.com/svctool.tar.gz",
            "abc",
        )],
        &super::formula::FormulaCode {
            install: "bin.install \"svctool\"",
            test: "system \"#{bin}/svctool\"",
        },
        &opts,
        &log,
    )
    .unwrap();
    assert!(
        formula.contains("\n  def plist\n    <<~EOS\n      <plist>stub</plist>\n    EOS\n  end\n"),
        "{formula}"
    );
    assert_eq!(
        capture.warn_messages(),
        [
            "DEPRECATION: homebrew formula 'svctool': `plist:` is deprecated; Homebrew \
          deprecated `def plist` in favour of `service do`, and Homebrew 7 removed the \
          `plist_options` stanza that went with it. anodizer writes `def plist` without \
          it; move the launchd settings to `service:`."
        ]
    );
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
        &test_log(),
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

/// Context carrying one darwin archive and a single top-level `homebrew_casks:`
/// entry, enough for `render_top_level_cask_entry` to produce a body.
fn top_cask_ctx(cask_cfg: &anodizer_core::config::HomebrewCaskConfig) -> Context {
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
    ctx
}

/// The cask token normaliser mirrors Homebrew's own: lowercase, spaces as
/// dashes, nothing else.
#[test]
fn cask_name_for_matches_goreleaser() {
    for (raw, want) in [
        ("SomeBinary", "somebinary"),
        ("Baton 1password", "baton-1password"),
        ("somE_binarY", "some_binary"),
        ("some binary@1", "some-binary@1"),
        ("binary", "binary"),
    ] {
        assert_eq!(
            super::cask::cask_name_for(raw),
            want,
            "cask token for {raw:?}"
        );
    }
}

/// A cask whose `name:` carries a capital and a space publishes under the
/// normalised token: the `.rb` stem and the `cask "..."` header are the same
/// string, so `brew install <token>` resolves the file that was written.
#[test]
fn cask_token_and_filename_use_the_normalised_name() {
    use anodizer_core::config::HomebrewCaskConfig;
    let cask_cfg = HomebrewCaskConfig {
        name: Some("Foo Bar".to_string()),
        ..Default::default()
    };
    let ctx = rendered_field_ctx(HomebrewConfig::default());
    let result = super::cask_scope::generate_cask_from_context(
        &ctx,
        "mytool",
        &HomebrewConfig::default(),
        &cask_cfg,
        &test_log(),
    )
    .expect("cask render");
    assert_eq!(
        result.cask_name, "foo-bar",
        "the `.rb` stem is the cask token"
    );
    assert!(
        result.content.contains("cask \"foo-bar\" do"),
        "cask header is not the normalised token:\n{}",
        result.content
    );

    // Same property on the top-level `homebrew_casks:` path, which renders
    // through `render_top_level_cask_inner` rather than the per-crate scope.
    let top_cfg = HomebrewCaskConfig {
        name: Some("Foo Bar".to_string()),
        ..Default::default()
    };
    let top = super::publish_top::render_top_level_cask_entry(
        &top_cask_ctx(&top_cfg),
        0,
        &top_cfg,
        &test_log(),
    )
    .expect("top-level cask render")
    .expect("cask applies");
    assert!(
        top.content.contains("cask \"foo-bar\" do"),
        "top-level cask header is not the normalised token:\n{}",
        top.content
    );
}

/// The `name "..."` stanza is Homebrew's human-readable name, so it keeps the
/// spelling the user configured even though the token is normalised.
#[test]
fn cask_display_name_keeps_the_raw_value() {
    use anodizer_core::config::HomebrewCaskConfig;
    let ctx = rendered_field_ctx(HomebrewConfig::default());
    let result = super::cask_scope::generate_cask_from_context(
        &ctx,
        "mytool",
        &HomebrewConfig::default(),
        &HomebrewCaskConfig {
            name: Some("Foo Bar".to_string()),
            ..Default::default()
        },
        &test_log(),
    )
    .expect("cask render");
    assert!(
        result.content.contains("name \"Foo Bar\""),
        "per-crate cask dropped the human-readable name:\n{}",
        result.content
    );

    let top_cfg = HomebrewCaskConfig {
        name: Some("Foo Bar".to_string()),
        ..Default::default()
    };
    let top = super::publish_top::render_top_level_cask_entry(
        &top_cask_ctx(&top_cfg),
        0,
        &top_cfg,
        &test_log(),
    )
    .expect("top-level cask render")
    .expect("cask applies");
    assert!(
        top.content.contains("name \"Foo Bar\""),
        "top-level cask dropped the human-readable name:\n{}",
        top.content
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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
/// own `on_arm` / `on_intel` sub-blocks carrying per-arch url + sha256. The
/// blocks come out in Homebrew's stanza order: `on_macos` before `on_linux`,
/// and `on_arm` before `on_intel` inside each.
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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

    let macos_pos = c.find("  on_macos do").expect("on_macos present");
    let linux_pos = c.find("  on_linux do").expect("on_linux present");
    assert!(
        macos_pos < linux_pos,
        "on_macos must precede on_linux (Homebrew stanza order):\n{c}"
    );
    // The two OS blocks form one stanza group, so no blank line parts them.
    assert!(
        c.contains("    end\n  end\n  on_linux do\n"),
        "on_linux must follow on_macos with no blank line:\n{c}"
    );

    for (block, start, end) in [
        ("on_macos", macos_pos, linux_pos),
        ("on_linux", linux_pos, c.len()),
    ] {
        let body = &c[start..end];
        let arm = body.find("on_arm do").expect("on_arm present");
        let intel = body.find("on_intel do").expect("on_intel present");
        assert!(
            arm < intel,
            "within {block}, on_arm must precede on_intel:\n{c}"
        );
    }
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
    let rendered = super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
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

/// One archive for `mytool` on `target`, with `amd64_variant` metadata when
/// a level is given.
fn variant_archive(target: &str, amd64_variant: Option<&str>) -> Artifact {
    let mut art = art_with_url_sha(
        ArtifactKind::Archive,
        &format!("mytool-{target}.tar.gz"),
        target,
        "https://ignored/original.tar.gz",
        &format!("sha_{target}"),
    );
    if let Some(v) = amd64_variant {
        art.metadata
            .insert("amd64_variant".to_string(), v.to_string());
    }
    art
}

/// A top-level `homebrew_casks` `url.template` renders the macOS artifact's
/// own target variant: the amd64 level its metadata records, and nothing for
/// a plain arm64 build (a darwin triple has no ABI component).
#[test]
fn homebrew_top_level_cask_url_template_renders_the_target_variant() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewCaskURL};
    let render = |target: &str, amd64_variant: Option<&str>| {
        let cask_cfg = HomebrewCaskConfig {
            name: Some("mytool".to_string()),
            url: Some(HomebrewCaskURL {
                template: Some(
                    "https://example.com/{{ Os }}_{{ Arch }}{{ targetVariant . }}".to_string(),
                ),
                ..Default::default()
            }),
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
        ctx.artifacts.add(variant_archive(target, amd64_variant));
        super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
            .expect("top-level cask render")
            .expect("cask applies")
            .content
    };
    let v3 = render("x86_64-apple-darwin", Some("v3"));
    assert!(
        v3.contains("url \"https://example.com/darwin_amd64v3\""),
        "{v3}"
    );
    let arm = render("aarch64-apple-darwin", None);
    assert!(
        arm.contains("url \"https://example.com/darwin_arm64\""),
        "{arm}"
    );
}

/// A per-crate cask `url_template` renders each artifact's own target
/// variant, in the multi-platform `on_macos` / `on_linux` blocks and in the
/// flat single-artifact body alike.
#[test]
fn homebrew_cask_url_template_renders_the_target_variant() {
    use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig};
    let render = |artifacts: Vec<Artifact>| {
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
        ctx.template_vars_mut().set("Version", "1.2.3");
        for art in artifacts {
            ctx.artifacts.add(art);
        }
        let cask_cfg = HomebrewCaskConfig {
            url_template: Some(
                "https://example.com/{{ Os }}_{{ Arch }}{{ targetVariant . }}".to_string(),
            ),
            ..Default::default()
        };
        super::cask_scope::generate_cask_from_context(
            &ctx,
            "mytool",
            &HomebrewConfig::default(),
            &cask_cfg,
            &test_log(),
        )
        .expect("cask render")
        .content
    };

    let multi = render(vec![
        variant_archive("x86_64-apple-darwin", Some("v3")),
        variant_archive("aarch64-apple-darwin", None),
        variant_archive("x86_64-unknown-linux-gnu", None),
        variant_archive("aarch64-unknown-linux-musl", None),
    ]);
    assert!(multi.contains("on_linux do"), "{multi}");
    for url in [
        "url \"https://example.com/darwin_amd64v3\"",
        "url \"https://example.com/darwin_arm64\"",
        "url \"https://example.com/linux_amd64_gnu\"",
        "url \"https://example.com/linux_arm64_musl\"",
    ] {
        assert!(multi.contains(url), "missing {url}\n{multi}");
    }

    let single = render(vec![variant_archive("x86_64-apple-darwin", Some("v3"))]);
    assert!(!single.contains("on_intel do"), "{single}");
    assert!(
        single.contains("url \"https://example.com/darwin_amd64v3\""),
        "{single}"
    );
    let arm = render(vec![variant_archive("aarch64-apple-darwin", None)]);
    assert!(
        arm.contains("url \"https://example.com/darwin_arm64\""),
        "{arm}"
    );
}

/// A `pull_request.token` that does not render disqualifies a crate's cask
/// before the tap is cloned: that tap keeps only its seed commit, the skip is
/// recorded, and the crate after it still pushes its cask.
#[test]
fn publish_cask_with_an_unrenderable_pull_request_token_skips_before_any_push() {
    let (broken_url, broken_bare) = make_bare_cask_tap("main");
    let (good_url, good_bare) = make_bare_cask_tap("main");
    let hb_for = |bare_url: &str| HomebrewConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("homebrew-tap".to_string()),
            branch: Some("main".to_string()),
            git: Some(anodizer_core::config::GitRepoConfig {
                url: Some(bare_url.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        cask: Some(HomebrewCaskConfig::default()),
        ..Default::default()
    };
    let mut broken_hb = hb_for(&broken_url);
    broken_hb.repository.as_mut().unwrap().pull_request =
        Some(anodizer_core::config::PullRequestConfig {
            enabled: None,
            base: None,
            draft: None,
            body: None,
            token: Some("{{ PrToken }}".to_string()),
        });
    let cask_crate = |name: &str, hb: HomebrewConfig| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            homebrew: Some(hb),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = Config {
        crates: vec![
            cask_crate("alpha", broken_hb),
            cask_crate("beta", hb_for(&good_url)),
        ],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("RawVersion", "1.2.3");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    for name in ["alpha", "beta"] {
        let mut a = art_with_url_sha(
            ArtifactKind::Archive,
            &format!("{name}-darwin.tar.gz"),
            "aarch64-apple-darwin",
            "https://e.com/x-darwin.tar.gz",
            "d".repeat(64).as_str(),
        );
        a.crate_name = name.to_string();
        ctx.artifacts.add(a);
    }

    super::publish_cask(&mut ctx, "alpha", &quiet_log())
        .expect("a token that does not render must skip, not fail");
    super::publish_cask(&mut ctx, "beta", &quiet_log()).expect("the next crate publishes");

    assert_eq!(
        anodizer_core::test_helpers::git_test_stdout(
            broken_bare.path(),
            &["rev-list", "--count", "--all"]
        ),
        "1",
        "the skipped crate's tap must keep only its seed commit"
    );
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].stage, "homebrew-cask");
    assert_eq!(events[0].label, "alpha");
    assert!(
        events[0]
            .reason
            .starts_with("pull_request.token does not render: "),
        "{}",
        events[0].reason
    );
    assert!(!events[0].reason.contains("{{"), "{}", events[0].reason);
    let landed = anodizer_core::test_helpers::git_test_stdout(
        good_bare.path(),
        &["show", "main:Casks/beta.rb"],
    );
    assert!(landed.contains("cask \"beta\""), "{landed}");
    drop(broken_bare);
    drop(good_bare);
}

/// A top-level cask whose `pull_request.token` does not render disqualifies
/// itself before its tap is cloned: that tap keeps only its seed commit, the
/// skip is recorded, and the entry after it still pushes.
#[test]
fn publish_top_level_casks_with_an_unrenderable_pull_request_token_skip_before_any_push() {
    let (broken_url, broken_bare) = make_bare_cask_tap("main");
    let (good_url, good_bare) = make_bare_cask_tap("main");
    let mut broken = local_cask_cfg(&broken_url, "main");
    broken.name = Some("broken".to_string());
    broken.repository.as_mut().unwrap().pull_request =
        Some(anodizer_core::config::PullRequestConfig {
            enabled: None,
            base: None,
            draft: None,
            body: None,
            token: Some("{{ PrToken }}".to_string()),
        });
    let mut healthy = local_cask_cfg(&good_url, "main");
    healthy.name = Some("healthy".to_string());
    let mut ctx = cask_publish_ctx(broken);
    ctx.config.homebrew_casks.as_mut().unwrap().push(healthy);

    let got = super::publish_top_level_homebrew_casks(&mut ctx, &quiet_log())
        .expect("a token that does not render must not fail the publisher");
    assert_eq!(got.total, 2);
    assert!(
        got.pushed_any,
        "the entry after the skipped one must push: {got:?}"
    );

    assert_eq!(
        anodizer_core::test_helpers::git_test_stdout(
            broken_bare.path(),
            &["rev-list", "--count", "--all"]
        ),
        "1",
        "the skipped entry's tap must keep only its seed commit"
    );
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].stage, "homebrew-cask");
    assert_eq!(events[0].label, "broken");
    assert!(
        events[0]
            .reason
            .starts_with("pull_request.token does not render: "),
        "{}",
        events[0].reason
    );
    assert!(!events[0].reason.contains("{{"), "{}", events[0].reason);
    let landed = anodizer_core::test_helpers::git_test_stdout(
        good_bare.path(),
        &["show", "main:Casks/healthy.rb"],
    );
    assert!(landed.contains("cask \"healthy\""), "{landed}");
    drop(broken_bare);
    drop(good_bare);
}

// -----------------------------------------------------------------------
// Rendered cask layouts checked by `brew style`
// -----------------------------------------------------------------------

/// The casks the layout tests and the `brew style` check render: a
/// macOS + Linux cask carrying every optional stanza, a macOS-only cask with
/// per-arch downloads, a flat single-url cask, a minimal one, a universal
/// macOS download beside Linux per-arch ones, a cask whose `custom_block`
/// declares its own macOS dependency, and a flat cask whose `custom_block`
/// holds an `on_linux` block. Returned as `(file stem, rendered cask)`.
fn style_fixture_casks() -> Vec<(&'static str, String)> {
    use super::cask::{
        ARCH_URL_ARGS_INDENT, CaskBinaryEntry, URL_ARGS_INDENT, render_additional_url_params,
        render_uninstall_block, render_zap_block,
    };
    let sha = |c: char| c.to_string().repeat(64);
    let release = "https://github.com/acme/mytool/releases/download/v#{version}";
    let arch = |a: &str, file: &str, c: char| CaskArchEntry {
        arch_block: a.to_string(),
        url: format!("{release}/{file}"),
        sha256: sha(c),
    };
    let url_cfg = HomebrewCaskURL {
        using: Some(":homebrew_curl".to_string()),
        headers: Some(vec![
            "Accept: application/octet-stream".to_string(),
            "X-Client: anodizer".to_string(),
        ]),
        ..Default::default()
    };
    let url_extras = render_additional_url_params(&url_cfg, URL_ARGS_INDENT);
    let url_extras_arch = render_additional_url_params(&url_cfg, ARCH_URL_ARGS_INDENT);
    let bins = vec![CaskBinaryEntry {
        name: "mytool".to_string(),
        target: None,
    }];
    let mans = vec!["mytool.1".to_string()];
    let alts = vec!["My Tool".to_string()];
    let deps = vec![
        "cask: \"other-app\"".to_string(),
        "formula: \"jq\"".to_string(),
    ];
    let conflicts = vec!["older-app".to_string(), "Old-App".to_string()];
    let uninstall = render_uninstall_block(Some(&HomebrewCaskUninstall {
        launchctl: Some(vec![
            "com.acme.mytool.helper".to_string(),
            "com.acme.mytool.agent".to_string(),
        ]),
        quit: Some(vec!["com.acme.mytool".to_string()]),
        ..Default::default()
    }));
    let zap = render_zap_block(Some(&HomebrewCaskUninstall {
        trash: Some(vec![
            "~/Library/Preferences/com.acme.mytool.plist".to_string(),
            "~/.mytool".to_string(),
        ]),
        ..Default::default()
    }));
    let completions = render_generate_completions(&HomebrewCaskGeneratedCompletions {
        executable: Some("mytool".to_string()),
        args: Some(vec!["completions".to_string()]),
        base_name: Some("mytool".to_string()),
        shell_parameter_format: Some("clap".to_string()),
        shells: Some(vec![
            "bash".to_string(),
            "zsh".to_string(),
            "fish".to_string(),
        ]),
    });

    let mut full = empty_cask_params("mytool", "1.2.3");
    full.alternative_names = &alts;
    full.homepage = Some("https://github.com/acme/mytool");
    full.description = Some("Release automation for Rust projects");
    full.app = Some("MyTool.app");
    full.service = Some("MyTool.workflow");
    full.binaries = &bins;
    full.manpages = &mans;
    full.caveats = Some("Run `mytool init` once.\n  Then `mytool release`.\n");
    full.zap_block = &zap;
    full.uninstall_block = &uninstall;
    full.livecheck = Some("url :url\nstrategy :github_latest".to_string());
    full.completions_bash = Some("completions/mytool.bash");
    full.completions_zsh = Some("completions/_mytool");
    full.completions_fish = Some("completions/mytool.fish");
    full.depends_on = &deps;
    full.conflicts_with = &conflicts;
    full.preflight = Some("system_command \"/usr/bin/true\"\n");
    full.postflight = Some(
        "if OS.mac?\n  system_command \"/usr/bin/xattr\", args: [\"-dr\", \"com.apple.quarantine\", \"#{staged_path}/mytool\"]\nend\n",
    );
    full.uninstall_preflight = Some("    system_command \"/usr/bin/true\"");
    full.uninstall_postflight = Some("system_command \"/usr/bin/true\"");
    full.generate_completions = completions;
    full.url_extras_indented = &url_extras_arch;
    // Supplied linux-first and intel-first, the order the artifact catalog
    // yields them, so the fixture shows the renderer reordering them.
    full.platforms = vec![
        CaskPlatformBlock {
            os_block: "linux".to_string(),
            arches: vec![
                arch("intel", "mytool-linux-amd64.tar.gz", 'c'),
                arch("arm", "mytool-linux-arm64.tar.gz", 'd'),
            ],
        },
        CaskPlatformBlock {
            os_block: "macos".to_string(),
            arches: vec![
                arch("intel", "mytool-darwin-amd64.tar.gz", 'a'),
                arch("arm", "mytool-darwin-arm64.tar.gz", 'b'),
            ],
        },
    ];

    let flat_uninstall = render_uninstall_block(Some(&HomebrewCaskUninstall {
        quit: Some(vec!["com.acme.mytool".to_string()]),
        ..Default::default()
    }));
    let flat_sha = sha('e');
    let flat_url = format!("{release}/mytool-darwin-universal.tar.gz");
    let mut flat = empty_cask_params("mytool", "1.2.3");
    flat.sha256 = &flat_sha;
    flat.url = &flat_url;
    flat.url_extras = &url_extras;
    flat.homepage = Some("https://github.com/acme/mytool");
    flat.description = Some("Release automation for Rust projects");
    flat.app = Some("MyTool.app");
    flat.binaries = &bins;
    flat.custom_block = Some("# Helpers for the hooks below.\n");
    flat.service = Some("MyTool.workflow");
    flat.caveats = Some("Run `mytool init` once.");
    flat.depends_on = &deps[..1];
    flat.conflicts_with = &conflicts[..1];
    flat.uninstall_block = &flat_uninstall;

    let min_sha = sha('f');
    let min_url = format!("{release}/mytool.tar.gz");
    let mut minimal = empty_cask_params("mytool", "1.2.3");
    minimal.sha256 = &min_sha;
    minimal.url = &min_url;
    minimal.binaries = &bins;

    let mut macos = empty_cask_params("mytool", "1.2.3");
    macos.homepage = Some("https://github.com/acme/mytool");
    macos.binaries = &bins;
    macos.platforms = vec![CaskPlatformBlock {
        os_block: "macos".to_string(),
        arches: vec![
            arch("intel", "mytool-darwin-amd64.tar.gz", 'a'),
            arch("arm", "mytool-darwin-arm64.tar.gz", 'b'),
        ],
    }];

    let mut universal = empty_cask_params("mytool", "1.2.3");
    universal.homepage = Some("https://github.com/acme/mytool");
    universal.app = Some("MyTool.app");
    universal.binaries = &bins;
    universal.url_extras_indented = &url_extras_arch;
    universal.platforms = vec![
        CaskPlatformBlock {
            os_block: "linux".to_string(),
            arches: vec![
                arch("intel", "mytool-linux-amd64.tar.gz", 'c'),
                arch("arm", "mytool-linux-arm64.tar.gz", 'd'),
            ],
        },
        CaskPlatformBlock {
            os_block: "macos".to_string(),
            arches: vec![arch(
                super::cask::UNIVERSAL_ARCH_BLOCK,
                "mytool-darwin-universal.tar.gz",
                'e',
            )],
        },
    ];

    let mut own_os_dependency = empty_cask_params("mytool", "1.2.3");
    own_os_dependency.sha256 = &flat_sha;
    own_os_dependency.url = &flat_url;
    own_os_dependency.homepage = Some("https://github.com/acme/mytool");
    own_os_dependency.app = Some("MyTool.app");
    own_os_dependency.custom_block = Some("depends_on macos: :sonoma");

    let mut custom_linux = empty_cask_params("mytool", "1.2.3");
    custom_linux.sha256 = &flat_sha;
    custom_linux.url = &flat_url;
    custom_linux.url_extras = &url_extras;
    custom_linux.url_extras_indented = &url_extras_arch;
    custom_linux.homepage = Some("https://github.com/acme/mytool");
    custom_linux.app = Some("MyTool.app");
    custom_linux.binaries = &bins;
    custom_linux.custom_block = Some("on_linux do\n  depends_on formula: \"jq\"\nend");

    // A plain `<<` heredoc closes only on a tag at column 0, and its body
    // is part of the text, so both must survive the re-indent.
    let mut custom_heredoc = empty_cask_params("mytool", "1.2.3");
    custom_heredoc.sha256 = &flat_sha;
    custom_heredoc.url = &flat_url;
    custom_heredoc.homepage = Some("https://github.com/acme/mytool");
    custom_heredoc.app = Some("MyTool.app");
    custom_heredoc.custom_block = Some("caveats <<EOS\n  Run mytool --setup once.\nEOS");

    vec![
        ("multi_platform", generate_cask(&full).unwrap()),
        ("macos_multi_arch", generate_cask(&macos).unwrap()),
        ("flat", generate_cask(&flat).unwrap()),
        ("minimal", generate_cask(&minimal).unwrap()),
        ("universal_and_linux", generate_cask(&universal).unwrap()),
        (
            "custom_block_depends_on",
            generate_cask(&own_os_dependency).unwrap(),
        ),
        (
            "custom_block_on_linux",
            generate_cask(&custom_linux).unwrap(),
        ),
        (
            "custom_block_plain_heredoc",
            generate_cask(&custom_heredoc).unwrap(),
        ),
    ]
}

fn style_fixture(stem: &str) -> String {
    style_fixture_casks()
        .into_iter()
        .find(|(name, _)| *name == stem)
        .map(|(_, body)| body)
        .unwrap_or_else(|| panic!("no style fixture named {stem}"))
}

/// The full macOS + Linux layout, byte for byte: `on_macos` before
/// `on_linux` and `on_arm` before `on_intel` whatever order the blocks
/// arrive in, the macOS-only `app` and `service` inside `on_macos`, one blank
/// line between stanza groups and none inside one, and `caveats` last with no
/// blank line before the closing `end`.
#[test]
fn multi_platform_cask_renders_in_homebrew_stanza_order() {
    assert_eq!(
        style_fixture("multi_platform"),
        r##"# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
cask "mytool" do
  version "1.2.3"

  on_macos do
    on_arm do
      sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      url "https://github.com/acme/mytool/releases/download/v#{version}/mytool-darwin-arm64.tar.gz",
          using:  :homebrew_curl,
          header: [
            "Accept: application/octet-stream",
            "X-Client: anodizer",
          ]
    end
    on_intel do
      sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      url "https://github.com/acme/mytool/releases/download/v#{version}/mytool-darwin-amd64.tar.gz",
          using:  :homebrew_curl,
          header: [
            "Accept: application/octet-stream",
            "X-Client: anodizer",
          ]
    end

    app "MyTool.app"
    service "MyTool.workflow"
  end
  on_linux do
    on_arm do
      sha256 "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
      url "https://github.com/acme/mytool/releases/download/v#{version}/mytool-linux-arm64.tar.gz",
          using:  :homebrew_curl,
          header: [
            "Accept: application/octet-stream",
            "X-Client: anodizer",
          ]
    end
    on_intel do
      sha256 "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
      url "https://github.com/acme/mytool/releases/download/v#{version}/mytool-linux-amd64.tar.gz",
          using:  :homebrew_curl,
          header: [
            "Accept: application/octet-stream",
            "X-Client: anodizer",
          ]
    end
  end

  name "mytool"
  name "My Tool"
  desc "Release automation for Rust projects"
  homepage "https://github.com/acme/mytool"

  livecheck do
    url :url
    strategy :github_latest
  end

  conflicts_with cask: [
    "Old-App",
    "older-app",
  ]
  depends_on cask: "other-app"
  depends_on formula: "jq"

  binary "mytool"
  manpage "mytool.1"
  bash_completion "completions/mytool.bash"
  zsh_completion "completions/_mytool"
  fish_completion "completions/mytool.fish"
  generate_completions_from_executable "mytool", "completions",
                                       base_name:              "mytool",
                                       shell_parameter_format: :clap,
                                       shells:                 [:bash, :zsh, :fish]

  preflight do
    system_command "/usr/bin/true"
  end

  postflight do
    if OS.mac?
      system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{staged_path}/mytool"]
    end
  end

  uninstall_preflight do
    system_command "/usr/bin/true"
  end

  uninstall_postflight do
    system_command "/usr/bin/true"
  end

  uninstall launchctl: [
              "com.acme.mytool.agent",
              "com.acme.mytool.helper",
            ],
            quit:      "com.acme.mytool"

  zap trash: [
    "~/.mytool",
    "~/Library/Preferences/com.acme.mytool.plist",
  ]

  caveats <<~EOS
    Run `mytool init` once.
      Then `mytool release`.
  EOS
end
"##
    );
}

/// The flat single-url layout, byte for byte: the custom block first, `url`
/// in the same group as `name`, `desc` and `homepage`, and `depends_on
/// :macos` because the only download is a macOS one.
#[test]
fn flat_cask_renders_in_homebrew_stanza_order() {
    assert_eq!(
        style_fixture("flat"),
        r##"# This file was generated by anodizer (https://github.com/tj-smith47/anodizer). DO NOT EDIT.
cask "mytool" do
  # Helpers for the hooks below.

  version "1.2.3"
  sha256 "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"

  url "https://github.com/acme/mytool/releases/download/v#{version}/mytool-darwin-universal.tar.gz",
      using:  :homebrew_curl,
      header: [
        "Accept: application/octet-stream",
        "X-Client: anodizer",
      ]
  name "mytool"
  desc "Release automation for Rust projects"
  homepage "https://github.com/acme/mytool"

  livecheck do
    skip "Auto-generated on release."
  end

  conflicts_with cask: "older-app"
  depends_on cask: "other-app"
  depends_on :macos

  app "MyTool.app"
  binary "mytool"
  service "MyTool.workflow"

  uninstall quit: "com.acme.mytool"

  caveats <<~EOS
    Run `mytool init` once.
  EOS
end
"##
    );
}

/// The environment variable that turns on [`rendered_casks_pass_brew_style_opt_in`].
const BREW_STYLE_OPT_IN: &str = "ANODIZER_BREW_STYLE";

/// Removes a directory tree when dropped, on a panic as well as on return.
struct RemoveTreeOnDrop(std::path::PathBuf);

impl Drop for RemoveTreeOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The file and offence counts from RuboCop's `1 file inspected, N offenses
/// detected` summary, or `None` when the report carries no summary.
fn rubocop_summary(report: &str) -> Option<(usize, usize)> {
    let line = report.lines().rev().find(|l| l.contains(" inspected, "))?;
    let (files, tail) = line.split_once(" inspected, ")?;
    let files = files.split_whitespace().next()?;
    let files = if files == "no" {
        0
    } else {
        files.parse().ok()?
    };
    let count = tail.split_whitespace().next()?;
    let offences = if count == "no" {
        0
    } else {
        count.parse().ok()?
    };
    Some((files, offences))
}

#[test]
fn rubocop_summary_reads_the_file_and_offence_counts() {
    assert_eq!(
        rubocop_summary("1 file inspected, no offenses detected\n"),
        Some((1, 0))
    );
    assert_eq!(
        rubocop_summary("x\n1 file inspected, 1 offense detected\n"),
        Some((1, 1))
    );
    assert_eq!(
        rubocop_summary("1 file inspected, 4 offenses detected, 4 offenses autocorrectable"),
        Some((1, 4))
    );
    assert_eq!(
        rubocop_summary("0 files inspected, no offenses detected\n"),
        Some((0, 0))
    );
    assert_eq!(
        rubocop_summary("no files inspected, no offenses detected\n"),
        Some((0, 0))
    );
    assert_eq!(rubocop_summary("no style gems installed"), None);
}

/// Run `brew style` over every layout fixture. Opt-in through
/// `ANODIZER_BREW_STYLE=1`, because it needs a working Homebrew with its
/// style gems (macOS, or Linux as a non-root user); once opted in, a brew
/// that cannot lint fails the test. `brew style` only lints a cask inside a
/// tap, so each fixture is written into a throwaway tap, trusted for the
/// test, that is untrusted and removed afterwards.
///
/// Two offences are allowed, each where its cause is the config's own Ruby:
///
/// - `Cask/InstallSteps` everywhere: hook bodies are raw Ruby from the
///   config, and Homebrew's `*_steps` replacement accepts only its own step
///   calls, so a cask with hooks keeps the legacy `preflight do` blocks.
/// - `Cask/StanzaOrder` in the `custom_block_*` casks only: `custom_block`
///   is placed first in the body, so a stanza written there sits ahead of
///   `version`. Those fixtures are there to show brew raises no
///   `Homebrew/OSDependsOn` offence beside the block's own dependency or
///   Linux block.
///
/// Every offence in RuboCop's summary count must be one of those, so an
/// offence of any severity (`C`, `W`, `E`, `F`, `R`) fails the test.
#[test]
fn rendered_casks_pass_brew_style_opt_in() {
    use anodizer_core::tool_detect::{ToolProbe, runs};
    if std::env::var(BREW_STYLE_OPT_IN).as_deref() != Ok("1") {
        println!(
            "skipped rendered_casks_pass_brew_style_opt_in: set {BREW_STYLE_OPT_IN}=1 to run brew style over the rendered casks"
        );
        return;
    }
    assert!(
        matches!(runs("brew"), ToolProbe::Available),
        "{BREW_STYLE_OPT_IN}=1 is set but brew is not runnable on PATH"
    );
    let brew = |args: &[&str]| {
        std::process::Command::new("brew")
            .args(args)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_INSTALL_FROM_API", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run brew")
    };
    let repo = brew(&["--repository"]);
    assert!(
        repo.status.success(),
        "brew --repository failed:\n{}",
        String::from_utf8_lossy(&repo.stderr)
    );
    // The guard below removes a tree under this path, so a blank or
    // relative answer must never reach it: it would resolve against the
    // working directory.
    let brew_repo = std::path::PathBuf::from(String::from_utf8_lossy(&repo.stdout).trim());
    assert!(
        !brew_repo.as_os_str().is_empty() && brew_repo.is_absolute(),
        "brew --repository printed {brew_repo:?}, which is not an absolute path"
    );
    // A per-process tap owner keeps concurrent runs apart. The guard exists
    // before the first directory does, so a partly created tap is removed
    // too, however the test ends.
    let owner_name = format!("anodizer-style-check-{}", std::process::id());
    let tap = format!("{owner_name}/check");
    let owner = brew_repo.join("Library/Taps").join(&owner_name);
    let _tap = RemoveTreeOnDrop(owner.clone());
    let casks = owner.join("homebrew-check/Casks");
    std::fs::create_dir_all(&casks).expect("create the throwaway tap");
    let cask_path = casks.join("mytool.rb");
    // Homebrew loads nothing from a tap nobody trusts, so the tap is
    // trusted for the test and untrusted afterwards.
    let trust = brew(&["trust", "--tap", &tap]);
    assert!(
        trust.status.success(),
        "brew trust --tap {tap} failed:\n{}",
        String::from_utf8_lossy(&trust.stderr)
    );
    let _trust = UntrustTapOnDrop(tap.clone());

    for (stem, body) in style_fixture_casks() {
        std::fs::write(&cask_path, &body).expect("write cask");
        let out = brew(&["style", "--cask", cask_path.to_str().expect("utf-8 path")]);
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        println!("=== {stem}\n{report}");
        let (files, total) = rubocop_summary(&report)
            .unwrap_or_else(|| panic!("brew style did not lint the {stem} cask:\n{report}"));
        assert_eq!(
            files, 1,
            "brew style inspected {files} files for the {stem} cask:\n{report}"
        );
        let allowed: &[&str] = if stem.starts_with("custom_block_") {
            &["Cask/InstallSteps:", "Cask/StanzaOrder:"]
        } else {
            &["Cask/InstallSteps:"]
        };
        let allowed_offences = report
            .lines()
            .filter(|l| allowed.iter().any(|cop| l.contains(cop)))
            .count();
        assert_eq!(
            total, allowed_offences,
            "brew style reported offences other than {allowed:?} for the {stem} cask:\n{report}\n--- cask ---\n{body}"
        );
    }
}

// -----------------------------------------------------------------------
// Rendered formula layouts checked by `brew style`
// -----------------------------------------------------------------------

/// The formulae the `brew style` check renders, one per layout the renderer
/// has: a lone archive for each of Apple Silicon, an Intel Mac and Linux,
/// macOS per-arch, an Intel-only macOS build (which carries the Apple
/// Silicon caveat, alone and beside configured caveats), an Apple
/// Silicon-only macOS build beside one Linux CPU, a universal macOS archive,
/// Linux per-arch beside a 32-bit archive that is left out, macOS + Linux
/// carrying every optional stanza, OS-scoped dependencies with no archive
/// for that OS, an archive for an OS Homebrew does not run on, a
/// `custom_block` that defines a method, a private download strategy, and
/// the legacy `plist`. `custom_block` is placed after `conflicts_with`, so
/// the full fixture's block holds a stanza Homebrew orders there. Returned
/// as `(file stem, rendered formula)`.
fn style_fixture_formulae() -> Vec<(&'static str, String)> {
    use super::formula::{FormulaCode, FormulaCore, render_formula_license};
    use anodizer_core::config::{HomebrewConflict, HomebrewDependency};
    let sha = |c: char| c.to_string().repeat(64);
    let (s1, s2, s3, s4, s5) = (sha('1'), sha('2'), sha('3'), sha('4'), sha('5'));
    let release = "https://github.com/acme/mytool/releases/download/v1.2.3";
    let url = |file: &str| format!("{release}/{file}");
    let (mac_intel, mac_arm, mac_universal) = (
        url("mytool-darwin-amd64.tar.gz"),
        url("mytool-darwin-arm64.tar.gz"),
        url("mytool-darwin-universal.tar.gz"),
    );
    let (linux_intel, linux_arm, linux_armv7) = (
        url("mytool-linux-amd64.tar.gz"),
        url("mytool-linux-arm64.tar.gz"),
        url("mytool-linux-armv7.tar.gz"),
    );
    let core = FormulaCore {
        name: "mytool",
        version: "1.2.3",
        description: "Fast CLI tool",
        license: "MIT",
    };
    let code = FormulaCode {
        install: "bin.install \"mytool\"",
        test: "system \"#{bin}/mytool\", \"--version\"",
    };
    let livecheck = Some("skip \"Auto-generated on release.\"".to_string());
    let base = || FormulaOptions {
        github_slug: Some("acme/mytool".to_string()),
        livecheck: livecheck.clone(),
        ..Default::default()
    };
    let dep = |name: &str, dep_type: Option<&str>, os: Option<&str>| HomebrewDependency {
        name: name.to_string(),
        os: os.map(str::to_string),
        dep_type: dep_type.map(str::to_string),
        version: None,
    };
    let deps = vec![
        dep("openssl@3", None, None),
        dep("pkg-config", Some("build"), None),
        dep("fzf", Some("optional"), None),
        dep("git", None, Some("mac")),
        dep("glibc", None, Some("linux")),
    ];
    let os_deps = vec![
        dep("git", None, Some("mac")),
        dep("glibc", None, Some("linux")),
    ];
    let conflicts = vec![
        HomebrewConflict::Name("mytool-nightly".to_string()),
        HomebrewConflict::WithReason {
            name: "othertool".to_string(),
            because: Some("both install a `mytool` binary".to_string()),
        },
    ];
    let headers = vec!["Accept: application/octet-stream".to_string()];
    let render = |archives: &[(&str, &str, &str)], opts: &FormulaOptions<'_>| {
        generate_formula_with_opts(&core, archives, &code, opts, &test_log())
            .expect("render formula")
    };

    let mut formulae = vec![
        (
            "single_apple_silicon",
            render(&[("aarch64-apple-darwin", &mac_arm, &s1)], &base()),
        ),
        (
            "single_intel_mac",
            render(&[("x86_64-apple-darwin", &mac_intel, &s1)], &base()),
        ),
        (
            "single_linux",
            render(&[("x86_64-unknown-linux-gnu", &linux_intel, &s1)], &base()),
        ),
        (
            "apple_silicon_only_and_linux_intel",
            render(
                &[
                    ("aarch64-apple-darwin", &mac_arm, &s1),
                    ("x86_64-unknown-linux-gnu", &linux_intel, &s2),
                ],
                &base(),
            ),
        ),
        (
            "unknown_os_entry",
            render(
                &[
                    (
                        "x86_64-unknown-freebsd",
                        &url("mytool-freebsd-amd64.tar.gz"),
                        &s1,
                    ),
                    ("aarch64-apple-darwin", &mac_arm, &s2),
                    ("x86_64-unknown-linux-gnu", &linux_intel, &s3),
                    ("aarch64-unknown-linux-gnu", &linux_arm, &s4),
                ],
                &base(),
            ),
        ),
    ];
    let mut custom_def = base();
    custom_def.custom_block =
        Some("def shell_completions\n  bash_completion.install \"completions/mytool.bash\"\nend");
    formulae.push((
        "custom_block_with_def",
        render(
            &[
                ("x86_64-apple-darwin", &mac_intel, &s1),
                ("aarch64-apple-darwin", &mac_arm, &s2),
            ],
            &custom_def,
        ),
    ));
    let mut custom_heredoc = base();
    custom_heredoc.custom_block =
        Some("def setup_note\n  <<EOS\n    Run mytool --setup once.\nEOS\nend");
    formulae.push((
        "custom_block_plain_heredoc",
        render(
            &[
                ("x86_64-apple-darwin", &mac_intel, &s1),
                ("aarch64-apple-darwin", &mac_arm, &s2),
            ],
            &custom_heredoc,
        ),
    ));
    formulae.push((
        "macos_per_arch",
        render(
            &[
                ("x86_64-apple-darwin", &mac_intel, &s1),
                ("aarch64-apple-darwin", &mac_arm, &s2),
            ],
            &base(),
        ),
    ));
    let intel_mac_and_linux = [
        ("x86_64-apple-darwin", mac_intel.as_str(), s1.as_str()),
        (
            "x86_64-unknown-linux-gnu",
            linux_intel.as_str(),
            s2.as_str(),
        ),
    ];
    formulae.push(("macos_intel_only", render(&intel_mac_and_linux, &base())));
    let mut with_caveats = base();
    with_caveats.caveats = Some("Run `mytool init` once.\nThen run `mytool serve`.");
    formulae.push((
        "macos_intel_only_with_caveats",
        render(&intel_mac_and_linux, &with_caveats),
    ));
    formulae.push((
        "macos_universal_and_linux",
        render(
            &[
                ("darwin-universal", &mac_universal, &s1),
                ("x86_64-unknown-linux-gnu", &linux_intel, &s2),
                ("aarch64-unknown-linux-gnu", &linux_arm, &s3),
            ],
            &base(),
        ),
    ));
    formulae.push((
        "linux_per_arch",
        render(
            &[
                ("x86_64-unknown-linux-gnu", &linux_intel, &s1),
                ("aarch64-unknown-linux-gnu", &linux_arm, &s2),
                ("armv7-unknown-linux-gnueabihf", &linux_armv7, &s3),
            ],
            &base(),
        ),
    ));
    let mut full = base();
    full.dependencies = Some(&deps);
    full.conflicts = Some(&conflicts);
    full.caveats = Some("Run `mytool init` once.");
    full.extra_install = Some("bash_completion.install \"completions/mytool.bash\"");
    full.post_install = Some("system bin/\"mytool\", \"setup\"");
    full.custom_block = Some("link_overwrite \"bin/mytool\"");
    full.service = Some("run [opt_bin/\"mytool\", \"serve\"]\nkeep_alive true");
    full.livecheck = Some("url :stable\nstrategy :github_latest".to_string());
    full.license_stanza = render_formula_license("MIT OR Apache-2.0");
    formulae.push((
        "macos_and_linux_full",
        render(
            &[
                ("x86_64-apple-darwin", &mac_intel, &s1),
                ("aarch64-apple-darwin", &mac_arm, &s2),
                ("x86_64-unknown-linux-gnu", &linux_intel, &s3),
                ("aarch64-unknown-linux-gnu", &linux_arm, &s4),
                ("armv7-unknown-linux-gnueabihf", &linux_armv7, &s5),
            ],
            &full,
        ),
    ));
    let mut os_deps_only = base();
    os_deps_only.dependencies = Some(&os_deps);
    formulae.push((
        "os_dependencies_without_their_archive",
        render(
            &[("x86_64-unknown-linux-gnu", &linux_intel, &s1)],
            &os_deps_only,
        ),
    ));
    let mut private = base();
    private.custom_require = Some("../lib/private_strategy");
    private.download_strategy = Some("GitHubPrivateRepositoryReleaseDownloadStrategy");
    private.url_headers = Some(&headers);
    formulae.push((
        "private_download_strategy",
        render(
            &[
                ("x86_64-apple-darwin", &mac_intel, &s1),
                ("aarch64-apple-darwin", &mac_arm, &s2),
                ("x86_64-unknown-linux-gnu", &linux_intel, &s3),
            ],
            &private,
        ),
    ));
    let mut plist = base();
    plist.plist =
        Some("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict/></plist>");
    formulae.push((
        "legacy_plist",
        render(&[("aarch64-apple-darwin", &mac_arm, &s1)], &plist),
    ));
    formulae
}

/// Run `brew style` over every formula layout fixture, the formula
/// counterpart of [`rendered_casks_pass_brew_style_opt_in`] under the same
/// `ANODIZER_BREW_STYLE=1` opt-in. Each fixture is written into a throwaway
/// tap, because `brew style` holds a tap formula to the tap rules, and the
/// tap is removed afterwards. `brew readall` then loads each one on every
/// OS and architecture Homebrew knows. Homebrew skips a tap nobody trusts,
/// so the tap is trusted for the test and untrusted afterwards, and a
/// readall that skipped anything fails.
///
/// Two offences are allowed, each where its cause is the config's own Ruby:
///
/// - `FormulaAudit/InstallSteps` in `macos_and_linux_full`: `post_install`
///   is raw Ruby from the config, and Homebrew's `post_install_steps`
///   replacement accepts only its own step calls, so the formula keeps
///   `def post_install`.
/// - `FormulaAudit/Text` (`def plist is deprecated`) in `legacy_plist`: the
///   `plist` field is that deprecated stanza, and `service` is its
///   replacement.
///
/// Every offence in RuboCop's summary count must be one of those, so an
/// offence of any severity (`C`, `W`, `E`, `F`, `R`) fails the test.
#[test]
fn rendered_formulae_pass_brew_style_opt_in() {
    use anodizer_core::tool_detect::{ToolProbe, runs};
    if std::env::var(BREW_STYLE_OPT_IN).as_deref() != Ok("1") {
        println!(
            "skipped rendered_formulae_pass_brew_style_opt_in: set {BREW_STYLE_OPT_IN}=1 to run brew style over the rendered formulae"
        );
        return;
    }
    assert!(
        matches!(runs("brew"), ToolProbe::Available),
        "{BREW_STYLE_OPT_IN}=1 is set but brew is not runnable on PATH"
    );
    let brew = |args: &[&str]| {
        std::process::Command::new("brew")
            .args(args)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_INSTALL_FROM_API", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run brew")
    };
    let repo = brew(&["--repository"]);
    assert!(
        repo.status.success(),
        "brew --repository failed:\n{}",
        String::from_utf8_lossy(&repo.stderr)
    );
    // The guard below removes a tree under this path, so a blank or
    // relative answer must never reach it.
    let brew_repo = std::path::PathBuf::from(String::from_utf8_lossy(&repo.stdout).trim());
    assert!(
        !brew_repo.as_os_str().is_empty() && brew_repo.is_absolute(),
        "brew --repository printed {brew_repo:?}, which is not an absolute path"
    );
    // The cask test runs in the same process, so the owner names the
    // formula check apart from its tap.
    let owner_name = format!("anodizer-formula-check-{}", std::process::id());
    let tap = format!("{owner_name}/check");
    let owner = brew_repo.join("Library/Taps").join(&owner_name);
    let _tap = RemoveTreeOnDrop(owner.clone());
    let tap_dir = owner.join("homebrew-check");
    let formula_dir = tap_dir.join("Formula");
    std::fs::create_dir_all(&formula_dir).expect("create the throwaway tap");
    let formula_path = formula_dir.join("mytool.rb");
    // The `private_download_strategy` fixture loads the strategy class
    // through `require_relative`, so the tap carries that file, outside
    // `Formula/`, where readall would load it as a formula.
    let strategy = tap_dir.join("lib/private_strategy.rb");
    std::fs::create_dir_all(strategy.parent().expect("lib dir")).expect("create lib");
    std::fs::write(
        &strategy,
        "# typed: false\n# frozen_string_literal: true\n\nclass GitHubPrivateRepositoryReleaseDownloadStrategy < CurlDownloadStrategy\nend\n",
    )
    .expect("write the download strategy");
    let trust = brew(&["trust", "--tap", &tap]);
    assert!(
        trust.status.success(),
        "brew trust --tap {tap} failed:\n{}",
        String::from_utf8_lossy(&trust.stderr)
    );
    let _trust = UntrustTapOnDrop(tap.clone());

    // Every fixture is linted before the test fails, so one run reports
    // the offences of all of them.
    let mut failures = Vec::new();
    for (stem, body) in style_fixture_formulae() {
        std::fs::write(&formula_path, &body).expect("write formula");
        let out = brew(&[
            "style",
            "--formula",
            formula_path.to_str().expect("utf-8 path"),
        ]);
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        println!("=== {stem}\n{report}");
        let (files, total) = rubocop_summary(&report)
            .unwrap_or_else(|| panic!("brew style did not lint the {stem} formula:\n{report}"));
        assert_eq!(
            files, 1,
            "brew style inspected {files} files for the {stem} formula:\n{report}"
        );
        let allowed: &[&str] = match stem {
            "macos_and_linux_full" => {
                &["FormulaAudit/InstallSteps: Formulae must use post_install_steps"]
            }
            "legacy_plist" => &["FormulaAudit/Text: def plist is deprecated."],
            _ => &[],
        };
        let allowed_offences = report
            .lines()
            .filter(|l| allowed.iter().any(|cop| l.contains(cop)))
            .count();
        if total != allowed_offences {
            failures.push(format!(
                "{stem}: offences other than {allowed:?}:\n{report}\n--- formula ---\n{body}"
            ));
        }
        let load = brew(&["readall", "--os=all", "--arch=all", &tap]);
        let load_report = format!(
            "{}{}",
            String::from_utf8_lossy(&load.stdout),
            String::from_utf8_lossy(&load.stderr)
        );
        println!("--- readall {stem}\n{load_report}");
        if !load.status.success() || load_report.contains("Skipping") {
            failures.push(format!(
                "{stem}: brew readall failed or skipped:\n{load_report}\n--- formula ---\n{body}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "brew style or brew readall failed for {} formulae:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Runs `brew untrust --tap` on the named tap when dropped, on a panic as
/// well as on return.
struct UntrustTapOnDrop(String);

impl Drop for UntrustTapOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("brew")
            .args(["untrust", "--tap", &self.0])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .stdin(std::process::Stdio::null())
            .output();
    }
}

/// Homebrew's formula component order (`FORMULA_COMPONENT_PRECEDENCE_LIST`
/// in Homebrew's `ast_constants.rb`, enforced by `FormulaAudit/ComponentsOrder`)
/// for the stanzas the renderer emits, at the top level and inside an
/// `on_macos` / `on_linux` block alike.
const FORMULA_COMPONENT_ORDER: &[&str] = &[
    "desc ",
    "homepage ",
    "url ",
    "version ",
    "sha256 ",
    "license ",
    "livecheck do",
    "depends_on ",
    "on_macos do",
    "on_linux do",
    "on_arm do",
    "on_intel do",
    "conflicts_with ",
    "def install",
    "def post_install",
    "def caveats",
    "def plist",
    "service do",
    "test do",
];

/// Every formula layout fixture lists its stanzas in Homebrew's component
/// order, at the top level of the class and inside each `on_macos` /
/// `on_linux` block, and defines each method once at the top level of the
/// class, the rules `brew style` holds a formula to that the renderer
/// decides. Runs without Homebrew, so the order holds where the opt-in
/// `brew style` check is not run.
#[test]
fn every_formula_layout_follows_the_homebrew_component_order() {
    for (stem, body) in style_fixture_formulae() {
        // The rank last seen at each indentation, the class body at 2 and an
        // OS block's body at 4. Leaving a block forgets the ranks inside it,
        // so the next block's order is checked on its own.
        let mut last: Vec<(usize, usize)> = Vec::new();
        let mut top_level = "";
        let mut os_block_stanzas = 0;
        for line in body.lines() {
            let stanza = line.trim_start();
            let indent = line.len() - stanza.len();
            if stanza.is_empty() {
                continue;
            }
            last.retain(|&(at, _)| at <= indent);
            if indent == 2 {
                top_level = stanza;
            }
            let in_os_block =
                indent == 4 && (top_level == "on_macos do" || top_level == "on_linux do");
            if indent != 2 && !in_os_block {
                continue;
            }
            let Some(rank) = FORMULA_COMPONENT_ORDER
                .iter()
                .position(|c| stanza.starts_with(c))
            else {
                continue;
            };
            os_block_stanzas += usize::from(in_os_block);
            if let Some(&(_, prev)) = last.iter().find(|&&(at, _)| at == indent) {
                assert!(
                    rank >= prev,
                    "{stem}: `{stanza}` follows `{}`:\n{body}",
                    FORMULA_COMPONENT_ORDER[prev]
                );
            }
            last.retain(|&(at, _)| at != indent);
            last.push((indent, rank));
        }
        if body.contains("\n  on_macos do\n") || body.contains("\n  on_linux do\n") {
            assert!(
                os_block_stanzas > 0,
                "{stem}: no OS block stanza was checked:\n{body}"
            );
        }
        for line in body.lines().filter(|l| l.trim_start().starts_with("def ")) {
            assert!(
                line.starts_with("  def ") && !line.starts_with("   "),
                "{stem}: `{}` is defined inside a block:\n{body}",
                line.trim()
            );
        }
        assert_eq!(
            body.matches("\n  def install\n").count(),
            1,
            "{stem}: one top-level install:\n{body}"
        );
    }
}

/// Every fenced block on a docs page as `(language, body)`, in page order.
/// A fence indented under a list item is read too, with that indent taken
/// off each body line.
pub(super) fn docs_fenced_blocks(page: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(String, usize, Vec<String>)> = None;
    for line in page.lines() {
        let text = line.trim_start();
        let indent = line.len() - text.len();
        match (text.strip_prefix("```"), open.take()) {
            (Some(_), Some((language, _, body))) => {
                let mut body = body.join("\n");
                body.push('\n');
                blocks.push((language, body));
            }
            (Some(language), None) => open = Some((language.to_string(), indent, Vec::new())),
            (None, Some((language, fence_indent, mut body))) => {
                body.push(line.get(fence_indent..).unwrap_or("").to_string());
                open = Some((language, fence_indent, body));
            }
            (None, None) => {}
        }
    }
    blocks
}

/// Every ```ruby block on the casks page is what the renderer produces for
/// the nearest ```yaml block above it. A block opening with the generated
/// header is a whole cask and must equal the render; any other block is a
/// run of lines the render must contain. The release artifacts behind each
/// render are listed per block in page order, so a new ```ruby block fails
/// the count until it is given its own.
#[test]
fn every_ruby_example_in_the_cask_docs_is_what_the_renderer_produces() {
    use anodizer_core::config::HomebrewCaskConfig;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/publish/homebrew-casks.md"
    );
    let page = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let blocks = docs_fenced_blocks(&page);

    let both_macs: &[&str] = &["x86_64-apple-darwin", "aarch64-apple-darwin"];
    let apple_silicon: &[&str] = &["aarch64-apple-darwin"];
    let universal_and_linux: &[&str] = &[
        "darwin-universal",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ];
    // The builds released beside each ```ruby block, in page order: the
    // dependencies fragment, the `depends_on_macos:` cask, the livecheck
    // fragment, the multi-arch cask, the single-arch cask, and the universal
    // binary beside Linux.
    let releases = [
        both_macs,
        apple_silicon,
        both_macs,
        both_macs,
        apple_silicon,
        universal_and_linux,
    ];

    let mut ruby_blocks = 0usize;
    for (at, (language, ruby)) in blocks.iter().enumerate() {
        if language != "ruby" {
            continue;
        }
        let targets = releases
            .get(ruby_blocks)
            .unwrap_or_else(|| panic!("ruby block {at} has no release listed for it in this test"));
        ruby_blocks += 1;
        let (_, yaml) = blocks[..at]
            .iter()
            .rev()
            .find(|(language, _)| language == "yaml")
            .unwrap_or_else(|| panic!("ruby block {at} has no yaml block above it"));
        let value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(yaml).unwrap_or_else(|e| panic!("yaml above {at}: {e}"));
        let entry = match value.get("homebrew_casks") {
            Some(list) => list[0].clone(),
            None => value,
        };
        let mut cask_cfg: HomebrewCaskConfig = serde_yaml_ng::from_value(entry)
            .unwrap_or_else(|e| panic!("yaml above {at} is not a cask entry: {e}"));
        cask_cfg.name.get_or_insert_with(|| "myapp".to_string());

        let mut ctx = top_cask_ctx(&cask_cfg);
        ctx.artifacts = Default::default();
        for target in *targets {
            let (sha, platform) = match *target {
                "x86_64-apple-darwin" => ('1', "darwin-amd64"),
                "aarch64-apple-darwin" => ('2', "darwin-arm64"),
                "darwin-universal" => ('3', "darwin-universal"),
                "x86_64-unknown-linux-gnu" => ('4', "linux-amd64"),
                "aarch64-unknown-linux-gnu" => ('5', "linux-arm64"),
                other => panic!("no artifact name for {other}"),
            };
            let name = format!("myapp-{platform}.tar.gz");
            ctx.artifacts.add(art_with_url_sha(
                ArtifactKind::Archive,
                &name,
                target,
                &format!("https://github.com/myorg/myapp/releases/download/v1.2.3/{name}"),
                &sha.to_string().repeat(64),
            ));
        }
        let rendered =
            super::publish_top::render_top_level_cask_entry(&ctx, 0, &cask_cfg, &test_log())
                .expect("top-level cask render")
                .expect("top-level cask applicable")
                .content;
        if ruby.starts_with(crate::util::GENERATED_FILE_HEADER) {
            assert!(
                *ruby == rendered,
                "ruby block {at} differs from the render of the yaml above it:\n{ruby}\n--- render ---\n{rendered}"
            );
        } else {
            assert!(
                rendered.contains(ruby.as_str()),
                "ruby block {at} is not part of the render of the yaml above it:\n{ruby}\n--- render ---\n{rendered}"
            );
        }
    }
    assert_eq!(ruby_blocks, releases.len(), "the page's ruby blocks");
}

/// A formula with no archive Homebrew can install names each archive and the
/// platform it was built for, so the reader sees what was skipped and why.
#[test]
fn a_formula_with_no_installable_archive_names_every_archive_and_platform() {
    let err = generate_formula(
        &super::formula::FormulaCore {
            name: "mytool",
            version: "1.0.0",
            description: "A tool",
            license: "MIT",
        },
        &[
            (
                "i686-unknown-linux-gnu",
                "https://e.com/mytool-linux-386.tar.gz",
                "a",
            ),
            (
                "x86_64-pc-windows-msvc",
                "https://e.com/mytool-windows-amd64.zip",
                "b",
            ),
        ],
        &super::formula::FormulaCode {
            install: "bin.install \"mytool\"",
            test: "system \"#{bin}/mytool\", \"--version\"",
        },
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "homebrew formula 'mytool': none of its archives is for a platform Homebrew \
         installs on (macOS, or Linux on x86_64 or arm64): \
         mytool-linux-386.tar.gz (linux/386), mytool-windows-amd64.zip (windows/amd64)"
    );
}

/// `reindent_code` moves only the lines that begin in code: a `<<` closer
/// stays at column 0 where Ruby needs it, and the text of a heredoc body
/// or a multi-line string is kept byte for byte.
#[test]
fn reindent_code_moves_only_the_lines_that_begin_in_code() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "a plain heredoc keeps its body and its closer at column 0",
            "caveats <<EOS\n    indented body\nEOS\nputs 1",
            "  caveats <<EOS\n    indented body\nEOS\n  puts 1",
        ),
        (
            "a dash heredoc keeps its body and its closer",
            "x = <<-EOS\nbody\n  EOS\ny = 2",
            "  x = <<-EOS\nbody\n  EOS\n  y = 2",
        ),
        (
            "a squiggly heredoc keeps its body and indents its closer",
            "x = <<~EOS\n  body\nEOS",
            "  x = <<~EOS\n  body\n  EOS",
        ),
        (
            "a multi-line string keeps its later lines",
            "desc \"one\ntwo\"\nurl \"x\"",
            "  desc \"one\ntwo\"\n  url \"x\"",
        ),
        (
            "code after a heredoc is indented",
            "if true\n  x = <<EOS\nb\nEOS\nend",
            "  if true\n    x = <<EOS\nb\nEOS\n  end",
        ),
        (
            "a =begin comment stays at column 0",
            "x = 1\n=begin\nnote\n=end\ny = 2",
            "  x = 1\n=begin\nnote\n=end\n  y = 2",
        ),
        (
            "past an unreadable line every line is kept",
            "x = 1\ny = \"open\n  z = 2",
            "  x = 1\n  y = \"open\n  z = 2",
        ),
    ];
    for (what, block, want) in cases {
        assert_eq!(super::formula::reindent_code(block, "  "), *want, "{what}");
    }
}

/// A line that opens a multi-line string keeps the spaces inside it, and a
/// line that ends in code loses its trailing whitespace, through both
/// re-indenting helpers. After a `-> { … }` body the names bound in it are
/// gone, so the literal a later line opens keeps its bytes.
#[test]
fn only_a_line_that_ends_in_code_is_trimmed() {
    let block = "desc \"trailing   \n  two\"\nurl \"x\"   ";
    assert_eq!(
        super::formula::reindent_code(block, "  "),
        "  desc \"trailing   \n  two\"\n  url \"x\""
    );
    assert_eq!(
        super::formula::indent_code_lines(block, "    "),
        ["    desc \"trailing   ", "  two\"", "    url \"x\""]
    );
    assert_eq!(
        super::formula::reindent_code("f = -> { w = 1 }\nr = w %(first\n  second)", "  "),
        "  f = -> { w = 1 }\n  r = w %(first\n  second)"
    );
}

/// A rendered cask keeps the whitespace that belongs to a literal: a
/// delimiter or content that is whitespace on a line that ends in code, the
/// body of a heredoc whose line ends in `\`, and a `\r` before the newline
/// inside a heredoc body. Only the whitespace past the last literal of a
/// code line is dropped.
#[test]
fn a_rendered_custom_block_keeps_the_whitespace_of_its_literals() {
    let cases: &[(&str, &str)] = &[
        ("sep = ?\\ ", "\n  sep = ?\\ \n"),
        ("s = % a ", "\n  s = % a \n"),
        ("s = %\ta\t", "\n  s = %\ta\t\n"),
        ("x = [%\nabc   \n]", "\n  x = [%\nabc   \n  ]\n"),
        ("s = \"a\"   ", "\n  s = \"a\"\n"),
        (
            "x = <<~EOS\n  a \\\nEOS\n  on_linux\nEOS",
            "\n  x = <<~EOS\n  a \\\nEOS\n  on_linux\n  EOS\n",
        ),
        (
            "x = <<-EOS\n  a \\\nEOS\n  on_linux\nEOS",
            "\n  x = <<-EOS\n  a \\\nEOS\n  on_linux\nEOS\n",
        ),
        (
            "x = <<~EOS\n  body  \r\nEOS\r\ny = 1  \r\n",
            "\n  x = <<~EOS\n  body  \r\n  EOS\r\n  y = 1\n",
        ),
    ];
    for (block, want) in cases {
        let mut params = empty_cask_params("mytool", "1.2.3");
        params.custom_block = Some(block);
        let cask = super::cask::generate_cask(&params).unwrap();
        assert!(cask.contains(want), "{block:?} rendered as:\n{cask}");
    }
}

/// `indent_code_lines` indents a code line, keeps a heredoc line as
/// written and leaves a blank line empty.
#[test]
fn indent_code_lines_indents_only_code_and_leaves_blank_lines_empty() {
    assert_eq!(
        super::formula::indent_code_lines("system <<EOS\n  ls\nEOS\n\nbin.install \"a\"", "    "),
        [
            "    system <<EOS",
            "  ls",
            "EOS",
            "",
            "    bin.install \"a\""
        ]
    );
}
