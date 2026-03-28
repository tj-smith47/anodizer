use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{run_cmd, run_cmd_in};

// ---------------------------------------------------------------------------
// Homebrew formula Tera template
// ---------------------------------------------------------------------------

const FORMULA_TEMPLATE: &str = r#"class {{ class_name }} < Formula
  desc "{{ description }}"
  homepage "https://github.com/{{ name }}"
  license "{{ license }}"
  version "{{ version }}"

{% if single_archive %}  url "{{ single_url }}"
  sha256 "{{ single_sha256 }}"
{% endif %}{% for entry in unknown_entries %}  # platform: {{ entry.platform }}
  url "{{ entry.url }}"
  sha256 "{{ entry.sha256 }}"
{% endfor %}{% if has_macos %}  on_macos do
{% if macos_has_arch %}{% for entry in macos_entries %}    {{ entry.arch_block }} do
      url "{{ entry.url }}"
      sha256 "{{ entry.sha256 }}"
    end
{% endfor %}{% else %}{% for entry in macos_entries %}    url "{{ entry.url }}"
    sha256 "{{ entry.sha256 }}"
{% endfor %}{% endif %}  end
{% endif %}{% if has_linux %}  on_linux do
{% if linux_has_arch %}{% for entry in linux_entries %}    {{ entry.arch_block }} do
      url "{{ entry.url }}"
      sha256 "{{ entry.sha256 }}"
    end
{% endfor %}{% else %}{% for entry in linux_entries %}    url "{{ entry.url }}"
    sha256 "{{ entry.sha256 }}"
{% endfor %}{% endif %}  end
{% endif %}
  def install
{% for line in install_lines %}    {{ line }}
{% endfor %}  end

  test do
{% for line in test_lines %}    {{ line }}
{% endfor %}  end
end
"#;

// ---------------------------------------------------------------------------
// generate_formula
// ---------------------------------------------------------------------------

/// Generate a Homebrew Ruby formula string.
///
/// `archives` is a slice of `(platform_tag, url, sha256)` tuples.
/// When there is a single archive entry (no `on_` OS block needed) the formula
/// uses a flat `url`/`sha256` pair; otherwise it emits an `on_macos`/`on_linux`
/// block per entry.
pub fn generate_formula(
    name: &str,
    version: &str,
    archives: &[(&str, &str, &str)],
    description: &str,
    license: &str,
    install: &str,
    test: &str,
) -> String {
    // Ruby class name: capitalise first letter, replace hyphens.
    let class_name: String = {
        let chars = name.replace('-', "_");
        chars
            .split('_')
            .map(|seg| {
                let mut c = seg.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let mut tera = tera::Tera::default();
    tera.add_raw_template("formula", FORMULA_TEMPLATE)
        .expect("homebrew: parse formula template");

    // Disable autoescaping (we're generating Ruby, not HTML)
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("class_name", &class_name);
    ctx.insert("name", name);
    ctx.insert("version", version);
    ctx.insert("description", description);
    ctx.insert("license", license);

    // Determine archive layout
    let single_archive = archives.len() == 1;
    ctx.insert("single_archive", &single_archive);

    if single_archive {
        ctx.insert("single_url", archives[0].1);
        ctx.insert("single_sha256", archives[0].2);
    } else {
        ctx.insert("single_url", "");
        ctx.insert("single_sha256", "");
    }

    // Build per-OS entry lists only for multi-archive layout
    let empty_vec: Vec<std::collections::HashMap<&str, &str>> = Vec::new();
    let (unknown_vals, macos_vals, linux_vals, macos_has_arch, linux_has_arch) = if single_archive {
        (
            empty_vec.clone(),
            empty_vec.clone(),
            empty_vec,
            false,
            false,
        )
    } else {
        let has_arch = |entries: &[(&str, &str, &str)]| -> bool {
            entries.iter().any(|(p, _, _)| {
                p.contains("arm64")
                    || p.contains("aarch64")
                    || p.contains("amd64")
                    || p.contains("x86_64")
            })
        };

        let unknown: Vec<_> = archives
            .iter()
            .filter(|(p, _, _)| {
                !p.contains("darwin") && !p.contains("macos") && !p.contains("linux")
            })
            .map(|(platform, url, sha256)| {
                let mut m = std::collections::HashMap::new();
                m.insert("platform", *platform);
                m.insert("url", *url);
                m.insert("sha256", *sha256);
                m
            })
            .collect();

        let macos_archives: Vec<_> = archives
            .iter()
            .filter(|(p, _, _)| p.contains("darwin") || p.contains("macos"))
            .copied()
            .collect();
        let macos_has = !macos_archives.is_empty() && has_arch(&macos_archives);
        let macos: Vec<_> = macos_archives
            .iter()
            .map(|(platform, url, sha256)| {
                let arch_block = if platform.contains("arm64") || platform.contains("aarch64") {
                    "on_arm"
                } else {
                    "on_intel"
                };
                let mut m = std::collections::HashMap::new();
                m.insert("url", *url);
                m.insert("sha256", *sha256);
                m.insert("arch_block", arch_block);
                m
            })
            .collect();

        let linux_archives: Vec<_> = archives
            .iter()
            .filter(|(p, _, _)| p.contains("linux"))
            .copied()
            .collect();
        let linux_has = !linux_archives.is_empty() && has_arch(&linux_archives);
        let linux: Vec<_> = linux_archives
            .iter()
            .map(|(platform, url, sha256)| {
                let arch_block = if platform.contains("arm64") || platform.contains("aarch64") {
                    "on_arm"
                } else {
                    "on_intel"
                };
                let mut m = std::collections::HashMap::new();
                m.insert("url", *url);
                m.insert("sha256", *sha256);
                m.insert("arch_block", arch_block);
                m
            })
            .collect();

        (unknown, macos, linux, macos_has, linux_has)
    };

    ctx.insert("unknown_entries", &unknown_vals);
    ctx.insert("has_macos", &!macos_vals.is_empty());
    ctx.insert("macos_has_arch", &macos_has_arch);
    ctx.insert("macos_entries", &macos_vals);
    ctx.insert("has_linux", &!linux_vals.is_empty());
    ctx.insert("linux_has_arch", &linux_has_arch);
    ctx.insert("linux_entries", &linux_vals);

    let install_lines: Vec<&str> = install.lines().collect();
    let test_lines: Vec<&str> = test.lines().collect();
    ctx.insert("install_lines", &install_lines);
    ctx.insert("test_lines", &test_lines);

    tera.render("formula", &ctx)
        .expect("homebrew: render formula template")
}

// ---------------------------------------------------------------------------
// publish_to_homebrew
// ---------------------------------------------------------------------------

pub fn publish_to_homebrew(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("homebrew: crate '{}' not found in config", crate_name))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no publish config for '{}'", crate_name))?;

    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no homebrew config for '{}'", crate_name))?;

    let tap = hb_cfg
        .tap
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no tap config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Homebrew tap {}/{} for '{}'",
            tap.owner, tap.name, crate_name
        ));
        return Ok(());
    }

    // Resolve version from template vars.
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    let description = hb_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let license = hb_cfg.license.clone().unwrap_or_else(|| "MIT".to_string());
    let install = hb_cfg
        .install
        .clone()
        .unwrap_or_else(|| format!("bin.install \"{}\"", crate_name));
    let test_block = hb_cfg
        .test
        .clone()
        .unwrap_or_else(|| format!("system \"#{{bin}}/{}\", \"--version\"", crate_name));

    // Collect Archive artifacts for this crate to build the formula entries.
    let archives: Vec<(&str, &str, &str)> = ctx
        .artifacts
        .by_kind_and_crate(anodize_core::artifact::ArtifactKind::Archive, crate_name)
        .iter()
        .filter_map(|a| {
            let url = a.metadata.get("url")?.as_str();
            let sha256 = a.metadata.get("sha256")?.as_str();
            let target = a.target.as_deref().unwrap_or("");
            Some((target, url, sha256))
        })
        .collect();

    let formula = generate_formula(
        crate_name,
        &version,
        &archives,
        &description,
        &license,
        &install,
        &test_block,
    );

    // Clone tap repo, write formula, commit, push.
    let tap_repo = format!("https://github.com/{}/{}.git", tap.owner, tap.name);
    let tmp_dir = tempfile::tempdir().context("homebrew: create temp dir")?;
    let repo_path = tmp_dir.path();

    // Determine the token for git auth.
    let token = ctx
        .options
        .token
        .clone()
        .or_else(|| std::env::var("HOMEBREW_TAP_TOKEN").ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let clone_url = if let Some(ref tok) = token {
        format!("https://{}@github.com/{}/{}.git", tok, tap.owner, tap.name)
    } else {
        tap_repo.clone()
    };

    run_cmd(
        "git",
        &[
            "clone",
            "--depth=1",
            &clone_url,
            &repo_path.to_string_lossy(),
        ],
        "homebrew: git clone",
    )?;

    // Determine formula folder.
    let folder = hb_cfg
        .folder
        .clone()
        .unwrap_or_else(|| "Formula".to_string());
    let formula_dir = repo_path.join(&folder);
    std::fs::create_dir_all(&formula_dir)
        .with_context(|| format!("homebrew: create formula dir {}", formula_dir.display()))?;

    let formula_path = formula_dir.join(format!("{}.rb", crate_name));
    std::fs::write(&formula_path, &formula)
        .with_context(|| format!("homebrew: write formula {}", formula_path.display()))?;

    log.status(&format!(
        "wrote Homebrew formula: {}",
        formula_path.display()
    ));

    // git add + commit + push
    run_cmd_in(
        repo_path,
        "git",
        &["add", &formula_path.to_string_lossy()],
        "homebrew: git add",
    )?;
    run_cmd_in(
        repo_path,
        "git",
        &[
            "commit",
            "-m",
            &format!("chore: update {} formula to {}", crate_name, version),
        ],
        "homebrew: git commit",
    )?;
    run_cmd_in(repo_path, "git", &["push"], "homebrew: git push")?;

    log.status(&format!(
        "Homebrew tap {}/{} updated for '{}'",
        tap.owner, tap.name, crate_name
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
        );
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
        );
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
        );
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
        );
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
            "anodize",
            "3.2.1",
            &[(
                "darwin-arm64",
                "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-darwin-arm64.tar.gz",
                "aabbccdd11223344",
            )],
            "Release automation for Rust projects",
            "Apache-2.0",
            "bin.install \"anodize\"",
            "system \"#{bin}/anodize\", \"--version\"",
        );

        // Verify class declaration
        assert!(
            formula.starts_with("class Anodize < Formula\n"),
            "should start with class declaration"
        );

        // Verify desc field
        assert!(formula.contains("  desc \"Release automation for Rust projects\"\n"));

        // Verify homepage
        assert!(formula.contains("  homepage \"https://github.com/anodize\"\n"));

        // Verify license
        assert!(formula.contains("  license \"Apache-2.0\"\n"));

        // Verify version
        assert!(formula.contains("  version \"3.2.1\"\n"));

        // Verify url and sha256 (single archive = flat, no on_macos block)
        assert!(formula.contains("  url \"https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-darwin-arm64.tar.gz\"\n"));
        assert!(formula.contains("  sha256 \"aabbccdd11223344\"\n"));

        // Verify install block
        assert!(formula.contains("  def install\n"));
        assert!(formula.contains("    bin.install \"anodize\"\n"));
        assert!(formula.contains("  end\n"));

        // Verify test block
        assert!(formula.contains("  test do\n"));
        assert!(formula.contains("    system \"#{bin}/anodize\", \"--version\"\n"));

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
        );

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
        assert!(
            formula.contains("      url \"https://example.com/my-cli-2.0.0-darwin-arm64.tar.gz\"")
        );
        assert!(formula.contains("      sha256 \"sha_darwin_arm64\""));

        // Verify install and test blocks are still present
        assert!(formula.contains("  def install\n"));
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
        );

        assert!(formula.contains("class EmptyTool < Formula"));
        assert!(formula.contains("  version \"0.1.0\""));
        // Should not contain any url/sha256 blocks
        assert!(!formula.contains("url \""));
        assert!(!formula.contains("sha256 \""));
        // But should still have install and test
        assert!(formula.contains("  def install\n"));
        assert!(formula.contains("  test do\n"));
    }

    #[test]
    fn test_publish_to_homebrew_dry_run() {
        use anodize_core::config::{Config, CrateConfig, HomebrewConfig, PublishConfig, TapConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let config = Config {
            crates: vec![CrateConfig {
                name: "cfgd".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig {
                        tap: Some(TapConfig {
                            owner: "myorg".to_string(),
                            name: "homebrew-tap".to_string(),
                        }),
                        description: Some("Declarative config management".to_string()),
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

        // dry-run should succeed without any network/git calls
        assert!(publish_to_homebrew(&ctx, "cfgd", &log).is_ok());
    }

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
        );

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
        );

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
        );

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
        );
        assert!(formula.contains("class MyCoolTool < Formula"));
    }
}
