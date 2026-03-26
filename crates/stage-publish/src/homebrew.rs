use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use std::process::Command;

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
        let mut chars = name.replace('-', "_");
        // PascalCase each segment
        chars = chars
            .split('_')
            .map(|seg| {
                let mut c = seg.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join("");
        chars
    };

    let mut f = String::new();
    f.push_str(&format!("class {} < Formula\n", class_name));
    f.push_str(&format!("  desc \"{}\"\n", description));
    f.push_str(&format!("  homepage \"https://github.com/{}\"\n", name));
    f.push_str(&format!("  license \"{}\"\n", license));
    f.push_str(&format!("  version \"{}\"\n", version));
    f.push('\n');

    match archives {
        [] => {}
        [(_, url, sha256)] => {
            f.push_str(&format!("  url \"{}\"\n", url));
            f.push_str(&format!("  sha256 \"{}\"\n", sha256));
        }
        entries => {
            // Group entries by OS block so that multiple arches for the same
            // OS (e.g. darwin-amd64 and darwin-arm64) end up inside a single
            // `on_macos do` block with nested `on_arm`/`on_intel` sub-blocks.

            // Collect unknown-platform entries first so they are emitted as
            // comments before the OS blocks.
            let mut unknown: Vec<(&str, &str, &str)> = Vec::new();
            let mut macos_entries: Vec<(&str, &str, &str)> = Vec::new();
            let mut linux_entries: Vec<(&str, &str, &str)> = Vec::new();

            for entry @ (platform, _url, _sha256) in entries {
                if platform.contains("darwin") || platform.contains("macos") {
                    macos_entries.push(*entry);
                } else if platform.contains("linux") {
                    linux_entries.push(*entry);
                } else {
                    unknown.push(*entry);
                }
            }

            for (platform, url, sha256) in &unknown {
                f.push_str(&format!(
                    "  # platform: {}\n  url \"{}\"\n  sha256 \"{}\"\n",
                    platform, url, sha256
                ));
            }

            for (os_block, os_entries) in [("on_macos", &macos_entries), ("on_linux", &linux_entries)] {
                if os_entries.is_empty() {
                    continue;
                }

                // Determine whether any entry has an explicit arch tag.
                let any_arch = os_entries.iter().any(|(platform, _, _)| {
                    platform.contains("arm64")
                        || platform.contains("aarch64")
                        || platform.contains("amd64")
                        || platform.contains("x86_64")
                });

                f.push_str(&format!("  {} do\n", os_block));

                if any_arch {
                    for (platform, url, sha256) in os_entries {
                        let arch_block =
                            if platform.contains("arm64") || platform.contains("aarch64") {
                                "on_arm"
                            } else {
                                "on_intel"
                            };
                        f.push_str(&format!("    {} do\n", arch_block));
                        f.push_str(&format!("      url \"{}\"\n", url));
                        f.push_str(&format!("      sha256 \"{}\"\n", sha256));
                        f.push_str("    end\n");
                    }
                } else {
                    // Single entry without explicit arch.
                    for (_platform, url, sha256) in os_entries {
                        f.push_str(&format!("    url \"{}\"\n", url));
                        f.push_str(&format!("    sha256 \"{}\"\n", sha256));
                    }
                }

                f.push_str("  end\n");
            }
        }
    }

    f.push('\n');
    f.push_str("  def install\n");
    for line in install.lines() {
        f.push_str(&format!("    {}\n", line));
    }
    f.push_str("  end\n");
    f.push('\n');
    f.push_str("  test do\n");
    for line in test.lines() {
        f.push_str(&format!("    {}\n", line));
    }
    f.push_str("  end\n");
    f.push_str("end\n");

    f
}

// ---------------------------------------------------------------------------
// publish_to_homebrew
// ---------------------------------------------------------------------------

pub fn publish_to_homebrew(ctx: &Context, crate_name: &str) -> Result<()> {
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
        eprintln!(
            "[publish] (dry-run) would update Homebrew tap {}/{} for '{}'",
            tap.owner, tap.name, crate_name
        );
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
    let license = hb_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());
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
        .by_kind_and_crate(
            anodize_core::artifact::ArtifactKind::Archive,
            crate_name,
        )
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
    let token = ctx.options.token.clone()
        .or_else(|| std::env::var("HOMEBREW_TAP_TOKEN").ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let clone_url = if let Some(ref tok) = token {
        format!(
            "https://{}@github.com/{}/{}.git",
            tok, tap.owner, tap.name
        )
    } else {
        tap_repo.clone()
    };

    run_cmd("git", &["clone", "--depth=1", &clone_url, &repo_path.to_string_lossy()], "homebrew: git clone")?;

    // Determine formula folder.
    let folder = hb_cfg.folder.clone().unwrap_or_else(|| "Formula".to_string());
    let formula_dir = repo_path.join(&folder);
    std::fs::create_dir_all(&formula_dir)
        .with_context(|| format!("homebrew: create formula dir {}", formula_dir.display()))?;

    let formula_path = formula_dir.join(format!("{}.rb", crate_name));
    std::fs::write(&formula_path, &formula)
        .with_context(|| format!("homebrew: write formula {}", formula_path.display()))?;

    eprintln!("[publish] wrote Homebrew formula: {}", formula_path.display());

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

    eprintln!(
        "[publish] Homebrew tap {}/{} updated for '{}'",
        tap.owner, tap.name, crate_name
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn run_cmd(program: &str, args: &[&str], context_msg: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
    Ok(())
}

fn run_cmd_in(dir: &std::path::Path, program: &str, args: &[&str], context_msg: &str) -> Result<()> {
    let status = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
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
            &[("darwin-amd64", "https://example.com/cfgd-1.0.0-darwin-amd64.tar.gz", "sha256abc")],
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
                ("darwin-amd64", "https://example.com/my-tool-2.0.0-darwin-amd64.tar.gz", "abc123"),
                ("linux-amd64", "https://example.com/my-tool-2.0.0-linux-amd64.tar.gz", "def456"),
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
                ("darwin-amd64", "https://example.com/mytool-darwin-amd64.tar.gz", "aaaa"),
                ("darwin-arm64", "https://example.com/mytool-darwin-arm64.tar.gz", "bbbb"),
                ("linux-amd64", "https://example.com/mytool-linux-amd64.tar.gz", "cccc"),
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
            &[("darwin-arm64", "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-darwin-arm64.tar.gz", "aabbccdd11223344")],
            "Release automation for Rust projects",
            "Apache-2.0",
            "bin.install \"anodize\"",
            "system \"#{bin}/anodize\", \"--version\"",
        );

        // Verify class declaration
        assert!(formula.starts_with("class Anodize < Formula\n"), "should start with class declaration");

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
        assert_eq!(end_lines.len(), 3, "should have 3 'end' lines: install, test, class");
    }

    #[test]
    fn test_integration_formula_multi_arch_complete_structure() {
        let formula = generate_formula(
            "my-cli",
            "2.0.0",
            &[
                ("darwin-arm64", "https://example.com/my-cli-2.0.0-darwin-arm64.tar.gz", "sha_darwin_arm64"),
                ("darwin-amd64", "https://example.com/my-cli-2.0.0-darwin-amd64.tar.gz", "sha_darwin_amd64"),
                ("linux-amd64", "https://example.com/my-cli-2.0.0-linux-amd64.tar.gz", "sha_linux_amd64"),
                ("linux-arm64", "https://example.com/my-cli-2.0.0-linux-arm64.tar.gz", "sha_linux_arm64"),
            ],
            "A CLI tool",
            "MIT",
            "bin.install \"my-cli\"",
            "system \"#{bin}/my-cli\", \"--version\"",
        );

        // Verify class name transforms hyphen to PascalCase
        assert!(formula.contains("class MyCli < Formula"));

        // Verify on_macos block with arch sub-blocks
        assert_eq!(formula.matches("on_macos do").count(), 1, "exactly one on_macos block");
        assert_eq!(formula.matches("on_linux do").count(), 1, "exactly one on_linux block");

        // Verify on_arm and on_intel are present inside macos
        assert!(formula.contains("on_arm do"), "should have on_arm block");
        assert!(formula.contains("on_intel do"), "should have on_intel block");

        // Verify all 4 URLs are present
        assert!(formula.contains("sha_darwin_arm64"));
        assert!(formula.contains("sha_darwin_amd64"));
        assert!(formula.contains("sha_linux_amd64"));
        assert!(formula.contains("sha_linux_arm64"));

        // Verify indentation of arch blocks (6 spaces for url/sha256 inside arch)
        assert!(formula.contains("      url \"https://example.com/my-cli-2.0.0-darwin-arm64.tar.gz\""));
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
}
