# Session 5: Extended Features — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement all extended features that round out anodize as a complete Rust release tool — Rust-specific features, auto-tagging, nightly builds, config includes, reproducible builds, macOS universal binaries, monorepo support, new publishers (Chocolatey, Winget, AUR, Krew), source archives, SBOM, UPX, new announce providers, CLI additions, maintenance, and documentation site.

**Architecture:** Each task is independent and touches distinct files/modules. Config additions go in `crates/core/src/config.rs`. New stages/publishers get their own modules in existing crates. New CLI commands go in `crates/cli/src/main.rs`. All follow existing patterns: `Stage` trait, `Context` passing, template rendering, dry-run safety.

**Tech Stack:** Rust 2024 edition, serde/serde_yaml for config, tera for templates, clap for CLI, anyhow for errors, mdBook for docs.

---

## Task 5A: Rust-Specific First-Class Features

**Files:**
- Modify: `crates/core/src/config.rs` — add `BinstallConfig`, `VersionSyncConfig`, build target type awareness
- Modify: `crates/stage-build/src/lib.rs` — handle cdylib/staticlib/wasm32 targets, version sync
- Create: `crates/stage-build/src/binstall.rs` — cargo-binstall metadata generation
- Create: `crates/stage-build/src/version_sync.rs` — Cargo.toml version syncing
- Modify: `crates/core/src/artifact.rs` — add `Library` and `Wasm` artifact kinds
- Modify: `crates/stage-build/Cargo.toml` — add `toml_edit` dependency

### 5A.1: Config additions

- [ ] **Step 1: Add config structs to `crates/core/src/config.rs`**

Add to `CrateConfig`:
```rust
pub binstall: Option<BinstallConfig>,
pub version_sync: Option<VersionSyncConfig>,
```

Add new structs:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BinstallConfig {
    pub enabled: Option<bool>,
    pub pkg_url: Option<String>,
    pub bin_dir: Option<String>,
    pub pkg_fmt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VersionSyncConfig {
    pub enabled: Option<bool>,
    pub mode: Option<String>, // "tag" (default) or "explicit"
}
```

Update `CrateConfig::default()` to include `binstall: None, version_sync: None`.

- [ ] **Step 2: Add config parsing tests**

In `crates/core/tests/config_parsing_tests.rs`, add:
```rust
#[test]
fn test_binstall_config_parsing() {
    let yaml = r#"
project_name: test
crates:
  - name: mycrate
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      pkg_url: "{ url }/{ name }-{ version }-{ target }.{ archive-format }"
      bin_dir: "{ bin }"
      pkg_fmt: "tgz"
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let binstall = config.crates[0].binstall.as_ref().unwrap();
    assert_eq!(binstall.enabled, Some(true));
    assert_eq!(binstall.pkg_fmt.as_deref(), Some("tgz"));
}

#[test]
fn test_version_sync_config_parsing() {
    let yaml = r#"
project_name: test
crates:
  - name: mycrate
    path: "."
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
      mode: tag
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let vs = config.crates[0].version_sync.as_ref().unwrap();
    assert_eq!(vs.enabled, Some(true));
    assert_eq!(vs.mode.as_deref(), Some("tag"));
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: all pass

### 5A.2: Artifact kinds for libraries and wasm

- [ ] **Step 4: Add artifact kinds to `crates/core/src/artifact.rs`**

Add to `ArtifactKind` enum:
```rust
Library,  // cdylib/staticlib output
Wasm,     // wasm32 target output
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace 2>&1 | tail -5`

### 5A.3: Version sync implementation

- [ ] **Step 6: Add `toml_edit` dependency**

Run: `cd /opt/repos/anodize && cargo add toml_edit --package anodize-stage-build`

- [ ] **Step 7: Create `crates/stage-build/src/version_sync.rs`**

```rust
use anyhow::{Context, Result};
use std::path::Path;

/// Update version in Cargo.toml to match the release tag version.
pub fn sync_version(crate_path: &Path, version: &str, dry_run: bool) -> Result<()> {
    let cargo_toml_path = crate_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    let current = doc
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if current == version {
        eprintln!("[version-sync] {} already at {}", cargo_toml_path.display(), version);
        return Ok(());
    }

    if dry_run {
        eprintln!(
            "[version-sync] (dry-run) would update {} from {} to {}",
            cargo_toml_path.display(),
            current,
            version
        );
        return Ok(());
    }

    doc["package"]["version"] = toml_edit::value(version);
    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    eprintln!(
        "[version-sync] updated {} from {} to {}",
        cargo_toml_path.display(),
        current,
        version
    );
    Ok(())
}
```

- [ ] **Step 8: Add `pub mod version_sync;` to `crates/stage-build/src/lib.rs`**

Add after the existing module declarations. Then in `BuildStage::run()`, before the build loop, add version sync logic:

```rust
// Version sync: update Cargo.toml version if configured
for krate in &ctx.config.crates {
    if let Some(vs) = &krate.version_sync {
        if vs.enabled.unwrap_or(false) {
            let version = ctx.template_vars().get("RawVersion")
                .unwrap_or_else(|| ctx.template_vars().get("Version").unwrap_or_default());
            if !version.is_empty() {
                version_sync::sync_version(
                    std::path::Path::new(&krate.path),
                    &version,
                    ctx.is_dry_run(),
                )?;
            }
        }
    }
}
```

- [ ] **Step 9: Add version sync test**

```rust
#[test]
fn test_version_sync_updates_cargo_toml() {
    let dir = tempfile::tempdir().unwrap();
    let cargo_toml = dir.path().join("Cargo.toml");
    std::fs::write(&cargo_toml, r#"[package]
name = "test"
version = "0.0.0"
edition = "2024"
"#).unwrap();

    version_sync::sync_version(dir.path(), "1.2.3", false).unwrap();

    let content = std::fs::read_to_string(&cargo_toml).unwrap();
    assert!(content.contains("version = \"1.2.3\""));
}

#[test]
fn test_version_sync_dry_run_does_not_modify() {
    let dir = tempfile::tempdir().unwrap();
    let cargo_toml = dir.path().join("Cargo.toml");
    std::fs::write(&cargo_toml, r#"[package]
name = "test"
version = "0.0.0"
edition = "2024"
"#).unwrap();

    version_sync::sync_version(dir.path(), "1.2.3", true).unwrap();

    let content = std::fs::read_to_string(&cargo_toml).unwrap();
    assert!(content.contains("version = \"0.0.0\""));
}
```

### 5A.4: cargo-binstall metadata generation

- [ ] **Step 10: Create `crates/stage-build/src/binstall.rs`**

```rust
use anodize_core::context::Context as AnodizeContext;
use anyhow::{Context, Result};
use std::path::Path;

/// Generate a cargo-binstall metadata section in Cargo.toml.
/// This enables `cargo binstall <crate>` to find pre-built binaries.
pub fn generate_binstall_metadata(
    crate_path: &Path,
    config: &anodize_core::config::BinstallConfig,
    ctx: &AnodizeContext,
    dry_run: bool,
) -> Result<()> {
    let cargo_toml_path = crate_path.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    let meta = doc
        .entry("package")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[package] is not a table"))?
        .entry("metadata")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[package.metadata] is not a table"))?
        .entry("binstall")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[package.metadata.binstall] is not a table"))?;

    if let Some(pkg_url) = &config.pkg_url {
        let rendered = ctx.render_template(pkg_url)?;
        meta.insert("pkg-url", toml_edit::value(&rendered));
    }
    if let Some(bin_dir) = &config.bin_dir {
        meta.insert("bin-dir", toml_edit::value(bin_dir.as_str()));
    }
    if let Some(pkg_fmt) = &config.pkg_fmt {
        meta.insert("pkg-fmt", toml_edit::value(pkg_fmt.as_str()));
    }

    if dry_run {
        eprintln!(
            "[binstall] (dry-run) would update {} with binstall metadata",
            cargo_toml_path.display()
        );
        return Ok(());
    }

    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    eprintln!(
        "[binstall] updated {} with binstall metadata",
        cargo_toml_path.display()
    );
    Ok(())
}
```

- [ ] **Step 11: Wire binstall into build stage and add `pub mod binstall;`**

In `BuildStage::run()`, after the version sync block:
```rust
// cargo-binstall metadata: update Cargo.toml if configured
for krate in &ctx.config.crates {
    if let Some(binstall_cfg) = &krate.binstall {
        if binstall_cfg.enabled.unwrap_or(false) {
            binstall::generate_binstall_metadata(
                std::path::Path::new(&krate.path),
                binstall_cfg,
                ctx,
                ctx.is_dry_run(),
            )?;
        }
    }
}
```

- [ ] **Step 12: Add cdylib/wasm32 awareness to build command construction**

In the build command construction, detect crate type from Cargo.toml and adjust:
- If target contains `wasm32`, use `--target wasm32-unknown-unknown` and look for `.wasm` output
- If crate type is `cdylib`, build with `--lib` and look for `.so`/`.dylib`/`.dll` output
- Register appropriate `ArtifactKind::Library` or `ArtifactKind::Wasm`

Add a helper function:
```rust
fn detect_crate_type(crate_path: &str) -> Option<String> {
    let cargo_toml_path = std::path::Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path).ok()?;
    let doc: toml::Value = toml::from_str(&content).ok()?;
    doc.get("lib")?
        .get("crate-type")?
        .as_array()?
        .first()?
        .as_str()
        .map(String::from)
}
```

- [ ] **Step 13: Add tests for binstall and cdylib detection**

```rust
#[test]
fn test_binstall_metadata_generation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), r#"[package]
name = "test"
version = "0.1.0"
edition = "2024"
"#).unwrap();

    let config = anodize_core::config::BinstallConfig {
        enabled: Some(true),
        pkg_url: Some("https://example.com/{ name }-{ version }-{ target }.{ archive-format }".to_string()),
        bin_dir: Some("{ bin }{ binary-ext }".to_string()),
        pkg_fmt: Some("tgz".to_string()),
    };

    // Create a minimal context for template rendering
    let mut cfg = anodize_core::config::Config::default();
    cfg.project_name = "test".to_string();
    let ctx = anodize_core::context::Context::new(cfg, Default::default());

    binstall::generate_binstall_metadata(dir.path(), &config, &ctx, false).unwrap();

    let content = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(content.contains("[package.metadata.binstall]"));
    assert!(content.contains("pkg-fmt = \"tgz\""));
}

#[test]
fn test_detect_crate_type_cdylib() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), r#"[package]
name = "test"
version = "0.1.0"
[lib]
crate-type = ["cdylib"]
"#).unwrap();

    assert_eq!(detect_crate_type(dir.path().to_str().unwrap()), Some("cdylib".to_string()));
}
```

- [ ] **Step 14: Run all tests and commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`

```bash
git add -A && git commit -m "feat: add Rust-specific features — cargo-binstall, version sync, cdylib/wasm32 support"
```

---

## Task 5B: Built-in Auto-Tagging (`anodize tag` command)

**Files:**
- Modify: `crates/cli/src/main.rs` — add `Tag` command variant
- Create: `crates/cli/src/commands/tag.rs` — auto-tag implementation
- Modify: `crates/core/src/config.rs` — add `TagConfig`
- Modify: `crates/core/src/git.rs` — add tag creation and push functions

### 5B.1: Config additions

- [ ] **Step 1: Add `TagConfig` to `crates/core/src/config.rs`**

Add to `Config` struct:
```rust
pub tag: Option<TagConfig>,
```

Add new struct:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TagConfig {
    pub default_bump: Option<String>,          // "patch", "minor", "major" (default: "minor")
    pub tag_prefix: Option<String>,            // default: "v"
    pub release_branches: Option<Vec<String>>, // branch patterns (regex)
    pub custom_tag: Option<String>,            // override all bump logic
    pub tag_context: Option<String>,           // "repo" or "branch" (default: "repo")
    pub branch_history: Option<String>,        // "compare", "last", "full" (default: "compare")
    pub initial_version: Option<String>,       // default: "0.0.0"
    pub prerelease: Option<bool>,              // default: false
    pub prerelease_suffix: Option<String>,     // default: "beta"
    pub force_without_changes: Option<bool>,   // default: false
    pub force_without_changes_pre: Option<bool>,
    pub major_string_token: Option<String>,    // default: "#major"
    pub minor_string_token: Option<String>,    // default: "#minor"
    pub patch_string_token: Option<String>,    // default: "#patch"
    pub none_string_token: Option<String>,     // default: "#none"
    pub git_api_tagging: Option<bool>,         // default: true (use GitHub API)
    pub verbose: Option<bool>,                 // default: true
}
```

Update `Config::default()` to include `tag: None`.

- [ ] **Step 2: Add config parsing tests**

```rust
#[test]
fn test_tag_config_full() {
    let yaml = r#"
project_name: test
tag:
  default_bump: patch
  tag_prefix: "v"
  release_branches: ["master", "release-.*"]
  tag_context: branch
  branch_history: last
  initial_version: "0.0.0"
  prerelease: false
  prerelease_suffix: beta
  major_string_token: "#major"
  minor_string_token: "#minor"
  patch_string_token: "#patch"
  none_string_token: "#none"
  force_without_changes: false
  git_api_tagging: true
  verbose: true
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let tag = config.tag.unwrap();
    assert_eq!(tag.default_bump.as_deref(), Some("patch"));
    assert_eq!(tag.tag_prefix.as_deref(), Some("v"));
    assert_eq!(tag.release_branches.as_ref().unwrap().len(), 2);
    assert_eq!(tag.branch_history.as_deref(), Some("last"));
}

#[test]
fn test_tag_config_minimal() {
    let yaml = r#"
project_name: test
tag: {}
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(config.tag.is_some());
}
```

### 5B.2: Git operations for tagging

- [ ] **Step 3: Add tag operations to `crates/core/src/git.rs`**

```rust
/// Get all semver tags matching an optional prefix, sorted by version descending.
pub fn get_all_semver_tags(prefix: &str) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["tag", "--list", &format!("{}*", prefix)])
        .output()
        .context("failed to run git tag --list")?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tags: Vec<(SemVer, String)> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|tag| {
            parse_semver(tag).ok().map(|sv| (sv, tag.to_string()))
        })
        .collect();

    tags.sort_by(|a, b| {
        b.0.major.cmp(&a.0.major)
            .then(b.0.minor.cmp(&a.0.minor))
            .then(b.0.patch.cmp(&a.0.patch))
    });

    Ok(tags.into_iter().map(|(_, tag)| tag).collect())
}

/// Get all semver tags reachable from the current branch only.
pub fn get_branch_semver_tags(prefix: &str) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["tag", "--list", &format!("{}*", prefix), "--merged", "HEAD"])
        .output()
        .context("failed to run git tag --list --merged")?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tags: Vec<(SemVer, String)> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|tag| {
            parse_semver(tag).ok().map(|sv| (sv, tag.to_string()))
        })
        .collect();

    tags.sort_by(|a, b| {
        b.0.major.cmp(&a.0.major)
            .then(b.0.minor.cmp(&a.0.minor))
            .then(b.0.patch.cmp(&a.0.patch))
    });

    Ok(tags.into_iter().map(|(_, tag)| tag).collect())
}

/// Create and push a git tag.
pub fn create_and_push_tag(tag: &str, message: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        eprintln!("[tag] (dry-run) would create tag: {}", tag);
        return Ok(());
    }

    let status = std::process::Command::new("git")
        .args(["tag", "-a", tag, "-m", message])
        .status()
        .context("failed to create git tag")?;

    if !status.success() {
        anyhow::bail!("git tag failed with exit code: {}", status);
    }

    let status = std::process::Command::new("git")
        .args(["push", "origin", tag])
        .status()
        .context("failed to push git tag")?;

    if !status.success() {
        anyhow::bail!("git push tag failed with exit code: {}", status);
    }

    Ok(())
}

/// Get the last N commit messages from HEAD.
pub fn get_last_commit_messages(count: usize) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["log", &format!("-{}", count), "--format=%s"])
        .output()
        .context("failed to run git log")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Get commit messages between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["log", &format!("{}..{}", from, to), "--format=%s"])
        .output()
        .context("failed to run git log")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Get current branch name.
pub fn get_current_branch() -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("failed to get current branch")?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
```

### 5B.3: Tag command implementation

- [ ] **Step 4: Create `crates/cli/src/commands/tag.rs`**

```rust
use anodize_core::config::TagConfig;
use anodize_core::git;
use anyhow::{Context, Result};
use regex::Regex;

pub struct TagOpts {
    pub dry_run: bool,
    pub custom_tag: Option<String>,
    pub default_bump: Option<String>,
    pub crate_name: Option<String>,
    pub verbose: bool,
}

pub struct TagResult {
    pub new_tag: String,
    pub old_tag: Option<String>,
    pub part: String, // "major", "minor", "patch", "none"
}

pub fn run(config: &anodize_core::config::Config, opts: TagOpts) -> Result<TagResult> {
    let tag_config = config.tag.clone().unwrap_or_default();

    // Resolve settings with CLI overrides
    let default_bump = opts
        .default_bump
        .or(tag_config.default_bump.clone())
        .unwrap_or_else(|| "minor".to_string());
    let prefix = tag_config.tag_prefix.clone().unwrap_or_else(|| "v".to_string());
    let tag_context = tag_config.tag_context.as_deref().unwrap_or("repo");
    let branch_history = tag_config.branch_history.as_deref().unwrap_or("compare");
    let initial_version = tag_config
        .initial_version
        .as_deref()
        .unwrap_or("0.0.0");
    let force = tag_config.force_without_changes.unwrap_or(false);
    let verbose = opts.verbose || tag_config.verbose.unwrap_or(true);

    let major_token = tag_config
        .major_string_token
        .as_deref()
        .unwrap_or("#major");
    let minor_token = tag_config
        .minor_string_token
        .as_deref()
        .unwrap_or("#minor");
    let patch_token = tag_config
        .patch_string_token
        .as_deref()
        .unwrap_or("#patch");
    let none_token = tag_config
        .none_string_token
        .as_deref()
        .unwrap_or("#none");

    // Handle custom tag override
    if let Some(custom) = opts.custom_tag.or(tag_config.custom_tag.clone()) {
        let tag_name = if custom.starts_with(&prefix) {
            custom
        } else {
            format!("{}{}", prefix, custom)
        };

        git::create_and_push_tag(&tag_name, &format!("Release {}", tag_name), opts.dry_run)?;

        return Ok(TagResult {
            new_tag: tag_name,
            old_tag: None,
            part: "custom".to_string(),
        });
    }

    // Check release branches
    if let Some(branches) = &tag_config.release_branches {
        let current = git::get_current_branch()?;
        let matches = branches.iter().any(|pattern| {
            Regex::new(pattern)
                .map(|re| re.is_match(&current))
                .unwrap_or(pattern == &current)
        });
        if !matches {
            eprintln!(
                "[tag] current branch '{}' does not match release branches {:?}",
                current, branches
            );
            // For non-release branches, generate a hash-postfixed version
            let short = git::detect_git_info("HEAD")
                .map(|gi| gi.short_commit)
                .unwrap_or_else(|_| "unknown".to_string());
            return Ok(TagResult {
                new_tag: format!("{}0.0.0-{}", prefix, short),
                old_tag: None,
                part: "none".to_string(),
            });
        }
    }

    // Find previous tag
    let tags = match tag_context {
        "branch" => git::get_branch_semver_tags(&prefix)?,
        _ => git::get_all_semver_tags(&prefix)?,
    };

    let old_tag = tags.first().cloned();
    let old_semver = old_tag
        .as_ref()
        .and_then(|t| git::parse_semver(t).ok())
        .unwrap_or_else(|| git::SemVer {
            major: initial_version.split('.').nth(0).and_then(|s| s.parse().ok()).unwrap_or(0),
            minor: initial_version.split('.').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0),
            patch: initial_version.split('.').nth(2).and_then(|s| s.parse().ok()).unwrap_or(0),
            prerelease: None,
        });

    // Check for changes since last tag
    if !force && old_tag.is_some() {
        let has_changes = git::get_commit_messages_between(
            old_tag.as_ref().unwrap(),
            "HEAD",
        )?;
        if has_changes.is_empty() {
            eprintln!("[tag] no commits since last tag, skipping (use force_without_changes to override)");
            return Ok(TagResult {
                new_tag: old_tag.clone().unwrap_or_default(),
                old_tag,
                part: "none".to_string(),
            });
        }
    }

    // Scan commit messages for bump directives
    let messages = match branch_history {
        "last" => git::get_last_commit_messages(1)?,
        "full" => {
            if let Some(ref ot) = old_tag {
                git::get_commit_messages_between(ot, "HEAD")?
            } else {
                git::get_last_commit_messages(100)?
            }
        }
        _ => {
            // "compare" — commits since previous tag
            if let Some(ref ot) = old_tag {
                git::get_commit_messages_between(ot, "HEAD")?
            } else {
                git::get_last_commit_messages(100)?
            }
        }
    };

    if verbose {
        for msg in &messages {
            eprintln!("[tag] commit: {}", msg);
        }
    }

    // Check for none token first
    if messages.iter().any(|m| m.contains(none_token)) {
        eprintln!("[tag] found {} — skipping", none_token);
        return Ok(TagResult {
            new_tag: old_tag.clone().unwrap_or_default(),
            old_tag,
            part: "none".to_string(),
        });
    }

    // Determine bump type
    let part = if messages.iter().any(|m| m.contains(major_token)) {
        "major"
    } else if messages.iter().any(|m| m.contains(minor_token)) {
        "minor"
    } else if messages.iter().any(|m| m.contains(patch_token)) {
        "patch"
    } else {
        &default_bump
    };

    let new_semver = match part {
        "major" => git::SemVer {
            major: old_semver.major + 1,
            minor: 0,
            patch: 0,
            prerelease: None,
        },
        "minor" => git::SemVer {
            major: old_semver.major,
            minor: old_semver.minor + 1,
            patch: 0,
            prerelease: None,
        },
        _ => git::SemVer {
            major: old_semver.major,
            minor: old_semver.minor,
            patch: old_semver.patch + 1,
            prerelease: None,
        },
    };

    // Handle prerelease
    let version_str = if tag_config.prerelease.unwrap_or(false) {
        let suffix = tag_config
            .prerelease_suffix
            .as_deref()
            .unwrap_or("beta");
        format!("{}.{}.{}-{}", new_semver.major, new_semver.minor, new_semver.patch, suffix)
    } else {
        format!("{}.{}.{}", new_semver.major, new_semver.minor, new_semver.patch)
    };

    let new_tag = format!("{}{}", prefix, version_str);

    eprintln!("[tag] {} → {} ({})", old_tag.as_deref().unwrap_or("(none)"), new_tag, part);

    git::create_and_push_tag(
        &new_tag,
        &format!("Release {}", new_tag),
        opts.dry_run,
    )?;

    // Print machine-parseable output for CI integration
    println!("new_tag={}", new_tag);
    println!("old_tag={}", old_tag.as_deref().unwrap_or(""));
    println!("part={}", part);

    Ok(TagResult {
        new_tag,
        old_tag,
        part: part.to_string(),
    })
}
```

- [ ] **Step 5: Add `Tag` command to CLI**

In `crates/cli/src/main.rs`, add to `Commands` enum:
```rust
/// Auto-tag based on commit message directives
Tag {
    #[arg(long)]
    dry_run: bool,
    #[arg(long, help = "Override bump logic with a specific tag value")]
    custom_tag: Option<String>,
    #[arg(long, help = "Override default bump type (patch/minor/major)")]
    default_bump: Option<String>,
    #[arg(long = "crate", help = "Tag a specific crate in a workspace")]
    crate_name: Option<String>,
},
```

In the `match` arm, add:
```rust
Commands::Tag { dry_run, custom_tag, default_bump, crate_name } => {
    let config_path = pipeline::find_config(cli.config.as_deref())?;
    let config = pipeline::load_config(&config_path)?;
    commands::tag::run(&config, commands::tag::TagOpts {
        dry_run,
        custom_tag,
        default_bump,
        crate_name,
        verbose: cli.verbose,
    })?;
}
```

Ensure `pub mod tag;` exists in `crates/cli/src/commands/mod.rs` (or create the mod if commands is a directory; if commands is a single file, restructure).

- [ ] **Step 6: Add tests for tag command**

Create tests in `crates/cli/tests/integration.rs`:
```rust
#[test]
fn test_tag_help_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["tag", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("--custom-tag"));
    assert!(stdout.contains("--default-bump"));
}
```

Add unit tests in `commands/tag.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_major() {
        let sv = git::SemVer { major: 1, minor: 2, patch: 3, prerelease: None };
        // Major bump: 1.2.3 → 2.0.0
        assert_eq!(sv.major + 1, 2);
    }

    #[test]
    fn test_none_token_detection() {
        let messages = vec!["fix: something #none".to_string()];
        let none_token = "#none";
        assert!(messages.iter().any(|m| m.contains(none_token)));
    }

    #[test]
    fn test_major_token_takes_precedence() {
        let messages = vec![
            "fix: something #patch".to_string(),
            "feat: big change #major".to_string(),
        ];
        let has_major = messages.iter().any(|m| m.contains("#major"));
        let has_patch = messages.iter().any(|m| m.contains("#patch"));
        assert!(has_major);
        assert!(has_patch);
        // Major should win
        let part = if has_major { "major" } else { "patch" };
        assert_eq!(part, "major");
    }
}
```

- [ ] **Step 7: Run tests and commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`

```bash
git add -A && git commit -m "feat: add anodize tag command with full anothrNick/github-tag-action parity"
```

---

## Task 5C: Nightly Builds (`--nightly`)

**Files:**
- Modify: `crates/cli/src/main.rs` — add `--nightly` flag to `Release`
- Modify: `crates/core/src/config.rs` — add `NightlyConfig`
- Modify: `crates/core/src/context.rs` — handle nightly version generation
- Modify: `crates/stage-release/src/lib.rs` — replace existing `nightly` release

- [ ] **Step 1: Add config**

In `config.rs`, add to `Config`:
```rust
pub nightly: Option<NightlyConfig>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NightlyConfig {
    pub name_template: Option<String>,  // default: "{{ ProjectName }}-nightly"
    pub tag_name: Option<String>,       // default: "nightly"
}
```

Update `Config::default()`.

- [ ] **Step 2: Add `--nightly` flag to CLI**

In `Commands::Release`, add:
```rust
#[arg(long, help = "Create a nightly release with date-based version")]
nightly: bool,
```

- [ ] **Step 3: Handle nightly in pipeline setup**

When `--nightly` is set:
- Generate version like `0.1.0-nightly.20260327` (from current date)
- Set `IsNightly` template variable to `"true"`
- Override tag to nightly config's `tag_name` (default: `"nightly"`)
- In release stage, delete existing release with same tag before creating new one

- [ ] **Step 4: Add nightly logic to context population**

In context setup (where snapshot logic already exists in the CLI commands), add nightly handling alongside snapshot:
```rust
if nightly {
    let date = chrono::Utc::now().format("%Y%m%d").to_string();
    let version = format!("{}-nightly.{}", base_version, date);
    ctx.template_vars_mut().set("Version", &version);
    ctx.template_vars_mut().set("IsNightly", "true");
    // Use nightly tag
    let tag_name = config.nightly.as_ref()
        .and_then(|n| n.tag_name.as_deref())
        .unwrap_or("nightly");
    ctx.template_vars_mut().set("Tag", tag_name);
}
```

- [ ] **Step 5: Update release stage for nightly replacement**

In `crates/stage-release/src/lib.rs`, when IsNightly is true, always set `replace_existing_draft: true` and `replace_existing_artifacts: true` behavior (delete old nightly release first).

- [ ] **Step 6: Add tests**

```rust
#[test]
fn test_nightly_config_parsing() {
    let yaml = r#"
project_name: test
nightly:
  name_template: "{{ ProjectName }}-nightly-build"
  tag_name: "nightly"
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let nightly = config.nightly.unwrap();
    assert_eq!(nightly.tag_name.as_deref(), Some("nightly"));
}
```

CLI integration test:
```rust
#[test]
fn test_release_help_shows_nightly_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["release", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--nightly"));
}
```

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat: add --nightly flag for automated rolling releases"
```

---

## Task 5D: Config Includes and Templates

**Files:**
- Modify: `crates/core/src/config.rs` — add `includes` field
- Modify: `crates/cli/src/pipeline.rs` — implement include merging in `load_config()`

- [ ] **Step 1: Add `includes` field to Config**

```rust
pub struct Config {
    pub includes: Option<Vec<String>>, // paths to YAML files to merge
    // ... existing fields
}
```

Update `Config::default()` with `includes: None`.

- [ ] **Step 2: Implement config merging in `pipeline.rs`**

Add a function to merge configs:
```rust
use serde_yaml::Value;

fn merge_yaml(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_yaml(existing, value),
                    None => { base_map.insert(key.clone(), value.clone()); }
                }
            }
        }
        (Value::Sequence(base_seq), Value::Sequence(overlay_seq)) => {
            base_seq.extend(overlay_seq.iter().cloned());
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}
```

In `load_config()`, after initial parse, process includes:
```rust
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;

    let mut base_value: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| "failed to parse config as YAML")?;

    // Process includes
    if let Some(includes) = base_value.get("includes").and_then(|v| v.as_sequence()) {
        let parent_dir = path.parent().unwrap_or(Path::new("."));
        let include_paths: Vec<String> = includes
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        for include_path in &include_paths {
            let resolved = parent_dir.join(include_path);
            let include_content = std::fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read include: {}", resolved.display()))?;
            let include_value: serde_yaml::Value = serde_yaml::from_str(&include_content)
                .with_context(|| format!("failed to parse include: {}", resolved.display()))?;
            merge_yaml(&mut base_value, &include_value);
        }
    }

    // Remove includes field before deserializing (it's not a Config field... or add it as one)
    let config: Config = serde_yaml::from_value(base_value)
        .with_context(|| "failed to deserialize merged config")?;

    Ok(config)
}
```

- [ ] **Step 3: Add tests**

```rust
#[test]
fn test_config_includes_merge() {
    let dir = tempfile::tempdir().unwrap();

    // Base config
    std::fs::write(dir.path().join(".anodize.yaml"), r#"
project_name: test
includes:
  - extra.yaml
crates:
  - name: base
    path: "."
    tag_template: "v{{ .Version }}"
"#).unwrap();

    // Include file
    std::fs::write(dir.path().join("extra.yaml"), r#"
changelog:
  sort: asc
  filters:
    exclude:
      - "^docs:"
"#).unwrap();

    let config = load_config(&dir.path().join(".anodize.yaml")).unwrap();
    assert_eq!(config.project_name, "test");
    assert!(config.changelog.is_some());
    assert_eq!(config.changelog.as_ref().unwrap().sort.as_deref(), Some("asc"));
}

#[test]
fn test_config_includes_deep_merge() {
    let mut base = serde_yaml::from_str::<serde_yaml::Value>(r#"
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  flags: "--release"
"#).unwrap();

    let overlay = serde_yaml::from_str::<serde_yaml::Value>(r#"
defaults:
  targets:
    - aarch64-apple-darwin
"#).unwrap();

    merge_yaml(&mut base, &overlay);

    let targets = base["defaults"]["targets"].as_sequence().unwrap();
    assert_eq!(targets.len(), 2); // Arrays concatenate
}
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: add config includes with deep merge support"
```

---

## Task 5E: Reproducible Builds (`SOURCE_DATE_EPOCH`)

**Files:**
- Modify: `crates/core/src/config.rs` — add `reproducible` field to `BuildConfig`
- Modify: `crates/stage-build/src/lib.rs` — set env vars for reproducible builds
- Modify: `crates/stage-archive/src/lib.rs` — strip timestamps from archives

- [ ] **Step 1: Add config field**

In `BuildConfig`:
```rust
pub reproducible: Option<bool>,
```

- [ ] **Step 2: Set SOURCE_DATE_EPOCH in build stage**

In build command construction, when `reproducible` is true:
```rust
if build_config.reproducible.unwrap_or(false) {
    if let Some(ts) = ctx.template_vars().get("CommitTimestamp") {
        cmd.env("SOURCE_DATE_EPOCH", ts);
    }
    // Add remap-path-prefix to strip local paths
    let rustflags = format!(
        "{} --remap-path-prefix={}=/build",
        std::env::var("RUSTFLAGS").unwrap_or_default(),
        std::env::current_dir().unwrap_or_default().display()
    );
    cmd.env("RUSTFLAGS", rustflags.trim());
}
```

- [ ] **Step 3: Strip timestamps in archive stage**

When creating tar archives, if any build has `reproducible: true`, set file modification times to `SOURCE_DATE_EPOCH`:
```rust
// In tar creation functions, when reproducible:
if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
    if let Ok(ts) = epoch.parse::<u64>() {
        header.set_mtime(ts);
    }
}
```

- [ ] **Step 4: Add tests**

```rust
#[test]
fn test_reproducible_build_config() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test
        reproducible: true
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].builds.as_ref().unwrap()[0].reproducible, Some(true));
}
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add reproducible build support with SOURCE_DATE_EPOCH"
```

---

## Task 5F: macOS Universal Binaries

**Files:**
- Modify: `crates/core/src/config.rs` — add `UniversalBinaryConfig`
- Modify: `crates/stage-build/src/lib.rs` — run `lipo` after building both arch targets
- Modify: `crates/core/src/artifact.rs` — handle universal binary artifacts

- [ ] **Step 1: Add config**

In `CrateConfig`:
```rust
pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UniversalBinaryConfig {
    pub name_template: Option<String>,
    pub replace: Option<bool>,  // replace individual arch binaries with universal
    pub ids: Option<Vec<String>>,
}
```

Update `CrateConfig::default()`.

- [ ] **Step 2: Implement lipo in build stage**

After all builds complete, check for universal binary configs:
```rust
// After main build loop, create universal binaries
for krate in &ctx.config.crates {
    if let Some(ub_configs) = &krate.universal_binaries {
        for ub in ub_configs {
            // Find aarch64-apple-darwin and x86_64-apple-darwin binaries
            let aarch64_bins: Vec<_> = ctx.artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|a| a.target.as_deref() == Some("aarch64-apple-darwin"))
                .collect();

            let x86_64_bins: Vec<_> = ctx.artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|a| a.target.as_deref() == Some("x86_64-apple-darwin"))
                .collect();

            for (arm_bin, x86_bin) in aarch64_bins.iter().zip(x86_64_bins.iter()) {
                let out_name = arm_bin.path.file_name().unwrap();
                let out_path = ctx.config.dist.join("universal").join(out_name);

                if ctx.is_dry_run() {
                    eprintln!("[build] (dry-run) would create universal binary: {}", out_path.display());
                    continue;
                }

                std::fs::create_dir_all(out_path.parent().unwrap())?;
                let status = std::process::Command::new("lipo")
                    .args([
                        "-create",
                        "-output",
                        out_path.to_str().unwrap(),
                        arm_bin.path.to_str().unwrap(),
                        x86_bin.path.to_str().unwrap(),
                    ])
                    .status()
                    .context("failed to run lipo — is Xcode installed?")?;

                if !status.success() {
                    anyhow::bail!("lipo failed with exit code: {}", status);
                }

                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Binary,
                    path: out_path,
                    target: Some("darwin-universal".to_string()),
                    crate_name: krate.name.clone(),
                    metadata: HashMap::from([
                        ("universal".to_string(), "true".to_string()),
                    ]),
                });
            }
        }
    }
}
```

- [ ] **Step 3: Add tests**

```rust
#[test]
fn test_universal_binary_config_parsing() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    universal_binaries:
      - replace: true
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let ub = config.crates[0].universal_binaries.as_ref().unwrap();
    assert_eq!(ub.len(), 1);
    assert_eq!(ub[0].replace, Some(true));
}
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: add macOS universal binary support via lipo"
```

---

## Task 5G: Monorepo Support

**Files:**
- Modify: `crates/core/src/config.rs` — add `workspaces` field
- Modify: `crates/cli/src/main.rs` — add `--workspace` flag
- Modify: `crates/cli/src/pipeline.rs` — workspace selection logic

- [ ] **Step 1: Add config**

In `Config`:
```rust
pub workspaces: Option<Vec<WorkspaceConfig>>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub name: String,
    pub dir: String,                           // relative path to workspace root
    pub crates: Vec<CrateConfig>,
    pub changelog: Option<ChangelogConfig>,
    pub release: Option<ReleaseConfig>,
}
```

Update `Config::default()`.

- [ ] **Step 2: Add `--workspace` flag to Release command**

```rust
#[arg(long, help = "Release a specific workspace in a monorepo")]
workspace: Option<String>,
```

- [ ] **Step 3: Wire workspace selection**

In the release command handler, when `--workspace` is provided, find the matching workspace config and use its crates/changelog/release instead of the top-level ones:
```rust
if let Some(ws_name) = &workspace {
    let ws = config.workspaces.as_ref()
        .and_then(|wss| wss.iter().find(|w| &w.name == ws_name))
        .ok_or_else(|| anyhow::anyhow!("workspace '{}' not found in config", ws_name))?;
    // Override config crates with workspace crates
    config.crates = ws.crates.clone();
    if let Some(cl) = &ws.changelog {
        config.changelog = Some(cl.clone());
    }
}
```

- [ ] **Step 4: Add tests**

```rust
#[test]
fn test_workspace_config_parsing() {
    let yaml = r#"
project_name: monorepo
workspaces:
  - name: frontend
    dir: apps/frontend
    crates:
      - name: frontend
        path: apps/frontend
        tag_template: "frontend-v{{ .Version }}"
  - name: backend
    dir: apps/backend
    crates:
      - name: backend
        path: apps/backend
        tag_template: "backend-v{{ .Version }}"
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let ws = config.workspaces.unwrap();
    assert_eq!(ws.len(), 2);
    assert_eq!(ws[0].name, "frontend");
    assert_eq!(ws[1].dir, "apps/backend");
}
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add monorepo support with independent workspaces"
```

---

## Task 5H: New Publishers — Chocolatey + Winget

**Files:**
- Create: `crates/stage-publish/src/chocolatey.rs`
- Create: `crates/stage-publish/src/winget.rs`
- Modify: `crates/stage-publish/src/lib.rs` — wire new publishers
- Modify: `crates/core/src/config.rs` — add `ChocolateyConfig`, `WingetConfig`

- [ ] **Step 1: Add config structs**

In `PublishConfig`:
```rust
pub chocolatey: Option<ChocolateyConfig>,
pub winget: Option<WingetConfig>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ChocolateyConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,                  // package name
    pub owners: Option<String>,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub project_url: Option<String>,
    pub icon_url: Option<String>,
    pub copyright: Option<String>,
    pub license_url: Option<String>,
    pub require_license_acceptance: Option<bool>,
    pub tags: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub release_notes: Option<String>,
    pub api_key: Option<String>,               // or use CHOCOLATEY_API_KEY env
    pub source_url: Option<String>,            // default: https://push.chocolateypackages.com/
    pub skip_publish: Option<bool>,            // generate but don't push
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WingetConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub publisher: Option<String>,
    pub publisher_url: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub license_url: Option<String>,
    pub copyright: Option<String>,
    pub tags: Option<Vec<String>>,
    pub repository: Option<String>,            // owner/winget-pkgs or custom
    pub skip_publish: Option<bool>,
    pub release_notes_url: Option<String>,
    pub package_identifier: Option<String>,    // e.g., "Owner.PackageName"
}
```

- [ ] **Step 2: Create `crates/stage-publish/src/chocolatey.rs`**

```rust
use anodize_core::artifact::ArtifactKind;
use anodize_core::config::ChocolateyConfig;
use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use std::path::Path;

pub fn publish_to_chocolatey(ctx: &mut Context, crate_name: &str) -> Result<()> {
    let krate = ctx.config.crates.iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("crate '{}' not found", crate_name))?;

    let choco_config = krate.publish.as_ref()
        .and_then(|p| p.chocolatey.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no chocolatey config for '{}'", crate_name))?;

    if !choco_config.enabled.unwrap_or(true) {
        return Ok(());
    }

    let pkg_name = choco_config.name.as_deref()
        .unwrap_or(&ctx.config.project_name);
    let version = ctx.template_vars().get("RawVersion")
        .unwrap_or_else(|| ctx.template_vars().get("Version").unwrap_or_default());

    // Find Windows archive artifact
    let windows_archive = ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .find(|a| a.target.as_deref().map(|t| t.contains("windows")).unwrap_or(false));

    // Generate .nuspec
    let nuspec = generate_nuspec(pkg_name, &version, choco_config, ctx)?;
    let nuspec_path = ctx.config.dist.join(format!("{}.nuspec", pkg_name));

    // Generate chocolateyInstall.ps1
    let install_script = generate_install_script(
        pkg_name,
        windows_archive.map(|a| a.path.display().to_string()).as_deref(),
        ctx,
    )?;
    let tools_dir = ctx.config.dist.join("tools");

    if ctx.is_dry_run() {
        eprintln!("[publish] (dry-run) chocolatey: would generate {} and push", nuspec_path.display());
        return Ok(());
    }

    std::fs::create_dir_all(&tools_dir)?;
    std::fs::write(&nuspec_path, &nuspec)?;
    std::fs::write(tools_dir.join("chocolateyInstall.ps1"), &install_script)?;

    eprintln!("[publish] chocolatey: generated {}", nuspec_path.display());

    if !choco_config.skip_publish.unwrap_or(false) {
        let api_key = choco_config.api_key.clone()
            .or_else(|| std::env::var("CHOCOLATEY_API_KEY").ok())
            .ok_or_else(|| anyhow::anyhow!("chocolatey: no API key (set CHOCOLATEY_API_KEY or publish.chocolatey.api_key)"))?;

        let source = choco_config.source_url.as_deref()
            .unwrap_or("https://push.chocolateypackages.com/");

        let status = std::process::Command::new("choco")
            .args(["push", nuspec_path.to_str().unwrap(), "--source", source, "--api-key", &api_key])
            .status()
            .context("failed to run choco push")?;

        if !status.success() {
            anyhow::bail!("choco push failed");
        }
    }

    Ok(())
}

fn generate_nuspec(name: &str, version: &str, config: &ChocolateyConfig, ctx: &Context) -> Result<String> {
    let mut xml = format!(r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>{}</id>
    <version>{}</version>
"#, name, version);

    if let Some(title) = &config.title { xml += &format!("    <title>{}</title>\n", title); }
    if let Some(authors) = &config.authors { xml += &format!("    <authors>{}</authors>\n", authors); }
    if let Some(owners) = &config.owners { xml += &format!("    <owners>{}</owners>\n", owners); }
    if let Some(summary) = &config.summary { xml += &format!("    <summary>{}</summary>\n", summary); }
    if let Some(desc) = &config.description {
        let rendered = ctx.render_template(desc)?;
        xml += &format!("    <description>{}</description>\n", rendered);
    }
    if let Some(url) = &config.project_url { xml += &format!("    <projectUrl>{}</projectUrl>\n", url); }
    if let Some(tags) = &config.tags { xml += &format!("    <tags>{}</tags>\n", tags); }
    if let Some(license) = &config.license_url { xml += &format!("    <licenseUrl>{}</licenseUrl>\n", license); }

    xml += "  </metadata>\n</package>";
    Ok(xml)
}

fn generate_install_script(name: &str, archive_path: Option<&str>, _ctx: &Context) -> Result<String> {
    let script = format!(r#"$ErrorActionPreference = 'Stop'
$toolsDir = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$packageName = '{}'
{}
"#, name,
        if let Some(path) = archive_path {
            format!(r#"
$url = '{}'
Install-ChocoBinaries -PackageName $packageName -Url $url -UnzipLocation $toolsDir
"#, path)
        } else {
            "Write-Warning 'No Windows archive found for this package'\n".to_string()
        }
    );
    Ok(script)
}
```

- [ ] **Step 3: Create `crates/stage-publish/src/winget.rs`**

```rust
use anodize_core::artifact::ArtifactKind;
use anodize_core::config::WingetConfig;
use anodize_core::context::Context;
use anyhow::{Context as _, Result};

pub fn publish_to_winget(ctx: &mut Context, crate_name: &str) -> Result<()> {
    let krate = ctx.config.crates.iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("crate '{}' not found", crate_name))?;

    let winget_config = krate.publish.as_ref()
        .and_then(|p| p.winget.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no winget config for '{}'", crate_name))?;

    if !winget_config.enabled.unwrap_or(true) {
        return Ok(());
    }

    let version = ctx.template_vars().get("RawVersion")
        .unwrap_or_else(|| ctx.template_vars().get("Version").unwrap_or_default());

    let pkg_id = winget_config.package_identifier.as_deref()
        .unwrap_or(&ctx.config.project_name);

    // Find Windows archive for download URL
    let windows_archives: Vec<_> = ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .filter(|a| a.target.as_deref().map(|t| t.contains("windows")).unwrap_or(false))
        .collect();

    let manifest = generate_winget_manifest(pkg_id, &version, winget_config, &windows_archives, ctx)?;
    let manifest_path = ctx.config.dist.join(format!("{}.{}.yaml", pkg_id, version));

    if ctx.is_dry_run() {
        eprintln!("[publish] (dry-run) winget: would generate {}", manifest_path.display());
        return Ok(());
    }

    std::fs::write(&manifest_path, &manifest)?;
    eprintln!("[publish] winget: generated {}", manifest_path.display());

    if !winget_config.skip_publish.unwrap_or(false) {
        eprintln!("[publish] winget: PR submission to winget-pkgs requires manual intervention or GitHub API integration");
    }

    Ok(())
}

fn generate_winget_manifest(
    pkg_id: &str,
    version: &str,
    config: &WingetConfig,
    _archives: &[&anodize_core::artifact::Artifact],
    ctx: &Context,
) -> Result<String> {
    let mut yaml = format!("PackageIdentifier: {}\nPackageVersion: {}\n", pkg_id, version);

    if let Some(name) = &config.name { yaml += &format!("PackageName: {}\n", name); }
    if let Some(publisher) = &config.publisher { yaml += &format!("Publisher: {}\n", publisher); }
    if let Some(license) = &config.license { yaml += &format!("License: {}\n", license); }
    if let Some(desc) = &config.short_description {
        let rendered = ctx.render_template(desc)?;
        yaml += &format!("ShortDescription: {}\n", rendered);
    }
    if let Some(tags) = &config.tags {
        yaml += "Tags:\n";
        for tag in tags {
            yaml += &format!("  - {}\n", tag);
        }
    }

    yaml += "ManifestType: singleton\nManifestVersion: 1.4.0\n";
    Ok(yaml)
}
```

- [ ] **Step 4: Wire into publish stage**

In `crates/stage-publish/src/lib.rs`, add:
```rust
pub mod chocolatey;
pub mod winget;
```

In `PublishStage::run()`, after scoop publishing:
```rust
// 4. Chocolatey
let choco_crates: Vec<String> = ctx.config.crates.iter()
    .filter(|c| selected.is_empty() || selected.contains(&c.name))
    .filter(|c| c.publish.as_ref().and_then(|p| p.chocolatey.as_ref()).is_some())
    .map(|c| c.name.clone())
    .collect();
for crate_name in &choco_crates {
    chocolatey::publish_to_chocolatey(ctx, crate_name)?;
}

// 5. Winget
let winget_crates: Vec<String> = ctx.config.crates.iter()
    .filter(|c| selected.is_empty() || selected.contains(&c.name))
    .filter(|c| c.publish.as_ref().and_then(|p| p.winget.as_ref()).is_some())
    .map(|c| c.name.clone())
    .collect();
for crate_name in &winget_crates {
    winget::publish_to_winget(ctx, crate_name)?;
}
```

- [ ] **Step 5: Add tests**

In `crates/stage-publish/src/chocolatey.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::*;

    #[test]
    fn test_generate_nuspec() {
        let config = ChocolateyConfig {
            title: Some("MyApp".to_string()),
            authors: Some("Author".to_string()),
            description: Some("A great app".to_string()),
            tags: Some("cli tool rust".to_string()),
            ..Default::default()
        };
        let mut cfg = Config::default();
        cfg.project_name = "myapp".to_string();
        let ctx = Context::new(cfg, Default::default());
        let nuspec = generate_nuspec("myapp", "1.0.0", &config, &ctx).unwrap();
        assert!(nuspec.contains("<id>myapp</id>"));
        assert!(nuspec.contains("<version>1.0.0</version>"));
        assert!(nuspec.contains("<title>MyApp</title>"));
    }
}
```

In `crates/stage-publish/src/winget.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::*;

    #[test]
    fn test_generate_winget_manifest() {
        let config = WingetConfig {
            name: Some("MyApp".to_string()),
            publisher: Some("MyOrg".to_string()),
            license: Some("MIT".to_string()),
            short_description: Some("A CLI tool".to_string()),
            tags: Some(vec!["cli".to_string(), "rust".to_string()]),
            ..Default::default()
        };
        let mut cfg = Config::default();
        cfg.project_name = "myapp".to_string();
        let ctx = Context::new(cfg, Default::default());
        let manifest = generate_winget_manifest("MyOrg.MyApp", "1.0.0", &config, &[], &ctx).unwrap();
        assert!(manifest.contains("PackageIdentifier: MyOrg.MyApp"));
        assert!(manifest.contains("PackageVersion: 1.0.0"));
        assert!(manifest.contains("Publisher: MyOrg"));
    }
}
```

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: add Chocolatey and Winget publisher support"
```

---

## Task 5I: New Publishers — AUR + Krew

**Files:**
- Create: `crates/stage-publish/src/aur.rs`
- Create: `crates/stage-publish/src/krew.rs`
- Modify: `crates/stage-publish/src/lib.rs` — wire new publishers
- Modify: `crates/core/src/config.rs` — add `AurConfig`, `KrewConfig`

- [ ] **Step 1: Add config structs**

In `PublishConfig`:
```rust
pub aur: Option<AurConfig>,
pub krew: Option<KrewConfig>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AurConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub maintainers: Option<Vec<String>>,
    pub contributors: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    pub optdepends: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
    pub git_url: Option<String>,           // AUR SSH URL
    pub commit_author: Option<String>,
    pub skip_publish: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KrewConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub caveats: Option<String>,
    pub index: Option<KrewIndexConfig>,
    pub skip_publish: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KrewIndexConfig {
    pub owner: String,
    pub name: String,
}
```

- [ ] **Step 2: Create `crates/stage-publish/src/aur.rs`**

Generate a PKGBUILD file:
```rust
use anodize_core::context::Context;
use anyhow::Result;

pub fn publish_to_aur(ctx: &mut Context, crate_name: &str) -> Result<()> {
    let krate = ctx.config.crates.iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("crate '{}' not found", crate_name))?;

    let aur_config = krate.publish.as_ref()
        .and_then(|p| p.aur.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no AUR config for '{}'", crate_name))?;

    if !aur_config.enabled.unwrap_or(true) {
        return Ok(());
    }

    let pkg_name = aur_config.name.as_deref().unwrap_or(crate_name);
    let version = ctx.template_vars().get("RawVersion").unwrap_or_default();
    let pkgbuild = generate_pkgbuild(pkg_name, &version, aur_config, ctx)?;
    let pkgbuild_path = ctx.config.dist.join(format!("PKGBUILD-{}", pkg_name));

    if ctx.is_dry_run() {
        eprintln!("[publish] (dry-run) AUR: would generate {}", pkgbuild_path.display());
        return Ok(());
    }

    std::fs::write(&pkgbuild_path, &pkgbuild)?;
    eprintln!("[publish] AUR: generated {}", pkgbuild_path.display());

    if !aur_config.skip_publish.unwrap_or(false) {
        eprintln!("[publish] AUR: push to AUR requires SSH access — use git_url config");
    }

    Ok(())
}

fn generate_pkgbuild(
    name: &str,
    version: &str,
    config: &anodize_core::config::AurConfig,
    ctx: &Context,
) -> Result<String> {
    let tag = ctx.template_vars().get("Tag").unwrap_or_default();
    let project = &ctx.config.project_name;

    let mut pkgbuild = format!(r#"# Maintainer: {}
pkgname={}
pkgver={}
pkgrel=1
pkgdesc='{}'
arch=('x86_64' 'aarch64')
url='https://github.com/{}/{}'
license=('{}')
"#,
        config.maintainers.as_ref().map(|m| m.join(", ")).unwrap_or_default(),
        name,
        version,
        config.description.as_deref().unwrap_or(""),
        ctx.template_vars().get("GitHubOwner").unwrap_or_default(),
        project,
        config.license.as_deref().unwrap_or("MIT"),
    );

    if let Some(deps) = &config.depends {
        pkgbuild += &format!("depends=({})\n", deps.iter().map(|d| format!("'{}'", d)).collect::<Vec<_>>().join(" "));
    }

    pkgbuild += &format!(r#"
source_x86_64=("${{pkgname}}-${{pkgver}}-x86_64.tar.gz::https://github.com/{owner}/{project}/releases/download/{tag}/${{pkgname}}-${{pkgver}}-x86_64-unknown-linux-gnu.tar.gz")
source_aarch64=("${{pkgname}}-${{pkgver}}-aarch64.tar.gz::https://github.com/{owner}/{project}/releases/download/{tag}/${{pkgname}}-${{pkgver}}-aarch64-unknown-linux-gnu.tar.gz")

package() {{
    install -Dm755 "${{srcdir}}/{name}" "${{pkgdir}}/usr/bin/{name}"
}}
"#,
        owner = ctx.template_vars().get("GitHubOwner").unwrap_or_default(),
        project = project,
        tag = tag,
        name = name,
    );

    Ok(pkgbuild)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::*;

    #[test]
    fn test_generate_pkgbuild() {
        let config = AurConfig {
            description: Some("A CLI tool".to_string()),
            license: Some("MIT".to_string()),
            maintainers: Some(vec!["user <user@example.com>".to_string()]),
            ..Default::default()
        };
        let mut cfg = Config::default();
        cfg.project_name = "myapp".to_string();
        let mut ctx = anodize_core::context::Context::new(cfg, Default::default());
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("GitHubOwner", "myorg");
        let pkgbuild = generate_pkgbuild("myapp", "1.0.0", &config, &ctx).unwrap();
        assert!(pkgbuild.contains("pkgname=myapp"));
        assert!(pkgbuild.contains("pkgver=1.0.0"));
        assert!(pkgbuild.contains("license=('MIT')"));
    }
}
```

- [ ] **Step 3: Create `crates/stage-publish/src/krew.rs`**

```rust
use anodize_core::artifact::ArtifactKind;
use anodize_core::config::KrewConfig;
use anodize_core::context::Context;
use anyhow::Result;

pub fn publish_to_krew(ctx: &mut Context, crate_name: &str) -> Result<()> {
    let krate = ctx.config.crates.iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("crate '{}' not found", crate_name))?;

    let krew_config = krate.publish.as_ref()
        .and_then(|p| p.krew.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no krew config for '{}'", crate_name))?;

    if !krew_config.enabled.unwrap_or(true) {
        return Ok(());
    }

    let name = krew_config.name.as_deref().unwrap_or(crate_name);
    let version = ctx.template_vars().get("RawVersion").unwrap_or_default();

    // Collect archives with their checksums for the manifest
    let archives = ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    let checksums = ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Checksum, crate_name);

    let manifest = generate_krew_manifest(name, &version, krew_config, &archives, &checksums, ctx)?;
    let manifest_path = ctx.config.dist.join(format!("{}.yaml", name));

    if ctx.is_dry_run() {
        eprintln!("[publish] (dry-run) krew: would generate {}", manifest_path.display());
        return Ok(());
    }

    std::fs::write(&manifest_path, &manifest)?;
    eprintln!("[publish] krew: generated {}", manifest_path.display());

    Ok(())
}

fn generate_krew_manifest(
    name: &str,
    version: &str,
    config: &KrewConfig,
    _archives: &[&anodize_core::artifact::Artifact],
    _checksums: &[&anodize_core::artifact::Artifact],
    _ctx: &Context,
) -> Result<String> {
    let mut yaml = format!(r#"apiVersion: krew.googlecontainertools.github.com/v1alpha2
kind: Plugin
metadata:
  name: {}
spec:
  version: v{}
  shortDescription: "{}"
"#, name, version, config.short_description.as_deref().unwrap_or(""));

    if let Some(desc) = &config.description {
        yaml += &format!("  description: |\n    {}\n", desc);
    }
    if let Some(homepage) = &config.homepage {
        yaml += &format!("  homepage: {}\n", homepage);
    }
    if let Some(caveats) = &config.caveats {
        yaml += &format!("  caveats: |\n    {}\n", caveats);
    }

    yaml += "  platforms: []\n"; // Platforms would be populated from archives

    Ok(yaml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::*;

    #[test]
    fn test_generate_krew_manifest() {
        let config = KrewConfig {
            short_description: Some("A kubectl plugin".to_string()),
            description: Some("Extended description".to_string()),
            homepage: Some("https://example.com".to_string()),
            ..Default::default()
        };
        let mut cfg = Config::default();
        cfg.project_name = "kubectl-myapp".to_string();
        let ctx = anodize_core::context::Context::new(cfg, Default::default());
        let manifest = generate_krew_manifest("myapp", "1.0.0", &config, &[], &[], &ctx).unwrap();
        assert!(manifest.contains("name: myapp"));
        assert!(manifest.contains("version: v1.0.0"));
        assert!(manifest.contains("A kubectl plugin"));
    }
}
```

- [ ] **Step 4: Wire into publish stage (same pattern as 5H Step 4)**

Add `pub mod aur; pub mod krew;` and corresponding crate iteration blocks.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add AUR and Krew publisher support"
```

---

## Task 5J: Source Archives + SBOM Generation

**Files:**
- Modify: `crates/core/src/config.rs` — add `SourceConfig`, `SbomConfig`
- Modify: `crates/core/src/artifact.rs` — add `SourceArchive`, `Sbom` artifact kinds
- Create: `crates/stage-archive/src/source.rs` — source archive generation
- Create: `crates/stage-archive/src/sbom.rs` — SBOM generation from Cargo.lock

- [ ] **Step 1: Add config and artifact kinds**

Config additions:
```rust
// In Config struct:
pub source: Option<SourceConfig>,
pub sbom: Option<SbomConfig>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SourceConfig {
    pub enabled: Option<bool>,
    pub format: Option<String>,       // "tar.gz" (default) or "zip"
    pub name_template: Option<String>,
    pub prefix_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SbomConfig {
    pub enabled: Option<bool>,
    pub format: Option<String>,       // "cyclonedx" (default) or "spdx"
}
```

Artifact kinds:
```rust
SourceArchive,
Sbom,
```

- [ ] **Step 2: Create `crates/stage-archive/src/source.rs`**

Generate source archive from git working tree:
```rust
use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anyhow::{Context as _, Result};

pub fn create_source_archive(ctx: &mut Context) -> Result<()> {
    let source_config = match &ctx.config.source {
        Some(c) if c.enabled.unwrap_or(false) => c.clone(),
        _ => return Ok(()),
    };

    let format = source_config.format.as_deref().unwrap_or("tar.gz");
    let name_template = source_config.name_template.as_deref()
        .unwrap_or("{{ ProjectName }}-{{ Version }}-source");
    let name = ctx.render_template(name_template)?;
    let filename = format!("{}.{}", name, format);
    let output_path = ctx.config.dist.join(&filename);

    if ctx.is_dry_run() {
        eprintln!("[source] (dry-run) would create source archive: {}", output_path.display());
        return Ok(());
    }

    std::fs::create_dir_all(&ctx.config.dist)?;

    // Use git archive to create source tarball (respects .gitignore)
    let git_format = match format {
        "zip" => "zip",
        _ => "tar.gz",
    };

    let prefix = source_config.prefix_template.as_deref()
        .map(|t| ctx.render_template(t))
        .transpose()?
        .unwrap_or_else(|| format!("{}/", name));

    let status = std::process::Command::new("git")
        .args([
            "archive",
            "--format", git_format,
            "--prefix", &prefix,
            "-o", output_path.to_str().unwrap(),
            "HEAD",
        ])
        .status()
        .context("failed to run git archive")?;

    if !status.success() {
        anyhow::bail!("git archive failed");
    }

    eprintln!("[source] created {}", output_path.display());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::SourceArchive,
        path: output_path,
        target: None,
        crate_name: ctx.config.project_name.clone(),
        metadata: std::collections::HashMap::from([
            ("format".to_string(), format.to_string()),
        ]),
    });

    Ok(())
}
```

- [ ] **Step 3: Create `crates/stage-archive/src/sbom.rs`**

Generate CycloneDX SBOM from Cargo.lock:
```rust
use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use serde_json::json;

pub fn generate_sbom(ctx: &mut Context) -> Result<()> {
    let sbom_config = match &ctx.config.sbom {
        Some(c) if c.enabled.unwrap_or(false) => c.clone(),
        _ => return Ok(()),
    };

    let format = sbom_config.format.as_deref().unwrap_or("cyclonedx");

    if ctx.is_dry_run() {
        eprintln!("[sbom] (dry-run) would generate {} SBOM", format);
        return Ok(());
    }

    let lock_path = std::path::Path::new("Cargo.lock");
    if !lock_path.exists() {
        eprintln!("[sbom] no Cargo.lock found, skipping SBOM generation");
        return Ok(());
    }

    let lock_content = std::fs::read_to_string(lock_path)?;
    let lock: toml::Value = toml::from_str(&lock_content)
        .context("failed to parse Cargo.lock")?;

    let packages = lock.get("package")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let sbom = match format {
        "cyclonedx" => generate_cyclonedx(&ctx.config.project_name, &packages)?,
        "spdx" => generate_spdx(&ctx.config.project_name, &packages)?,
        _ => anyhow::bail!("unsupported SBOM format: {}", format),
    };

    let ext = match format {
        "cyclonedx" => "cdx.json",
        "spdx" => "spdx.json",
        _ => "json",
    };
    let output_path = ctx.config.dist.join(format!("{}-sbom.{}", ctx.config.project_name, ext));
    std::fs::create_dir_all(&ctx.config.dist)?;
    std::fs::write(&output_path, &sbom)?;

    eprintln!("[sbom] generated {}", output_path.display());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Sbom,
        path: output_path,
        target: None,
        crate_name: ctx.config.project_name.clone(),
        metadata: std::collections::HashMap::from([
            ("format".to_string(), format.to_string()),
        ]),
    });

    Ok(())
}

fn generate_cyclonedx(project: &str, packages: &[toml::Value]) -> Result<String> {
    let components: Vec<_> = packages.iter()
        .filter_map(|pkg| {
            let name = pkg.get("name")?.as_str()?;
            let version = pkg.get("version")?.as_str()?;
            Some(json!({
                "type": "library",
                "name": name,
                "version": version,
                "purl": format!("pkg:cargo/{}@{}", name, version),
            }))
        })
        .collect();

    let bom = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": project,
            }
        },
        "components": components,
    });

    serde_json::to_string_pretty(&bom).map_err(Into::into)
}

fn generate_spdx(project: &str, packages: &[toml::Value]) -> Result<String> {
    let spdx_packages: Vec<_> = packages.iter()
        .filter_map(|pkg| {
            let name = pkg.get("name")?.as_str()?;
            let version = pkg.get("version")?.as_str()?;
            Some(json!({
                "SPDXID": format!("SPDXRef-{}-{}", name, version),
                "name": name,
                "versionInfo": version,
                "downloadLocation": "NOASSERTION",
                "externalRefs": [{
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": format!("pkg:cargo/{}@{}", name, version),
                }]
            }))
        })
        .collect();

    let doc = json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": project,
        "packages": spdx_packages,
    });

    serde_json::to_string_pretty(&doc).map_err(Into::into)
}
```

- [ ] **Step 4: Wire into archive stage**

In `crates/stage-archive/src/lib.rs`, add `pub mod source; pub mod sbom;` and call them at the end of `ArchiveStage::run()`:
```rust
source::create_source_archive(ctx)?;
sbom::generate_sbom(ctx)?;
```

- [ ] **Step 5: Add tests and commit**

Tests for source archive config parsing, SBOM generation from a sample Cargo.lock, and CycloneDX/SPDX format validation.

```bash
git add -A && git commit -m "feat: add source archive and SBOM generation (CycloneDX/SPDX)"
```

---

## Task 5K: UPX Binary Compression

**Files:**
- Modify: `crates/core/src/config.rs` — add `UpxConfig`
- Modify: `crates/stage-build/src/lib.rs` — add UPX post-processing step

- [ ] **Step 1: Add config**

In `Config`:
```rust
#[serde(default, deserialize_with = "deserialize_upx")]
pub upx: Vec<UpxConfig>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpxConfig {
    pub enabled: Option<bool>,
    pub ids: Option<Vec<String>>,
    pub binary: Option<String>,        // path to upx binary (default: "upx")
    pub compress: Option<String>,      // compression level: "best", "1"-"9", etc.
    pub lzma: Option<bool>,
    pub brute: Option<bool>,
    pub goos: Option<Vec<String>>,     // OS filter
    pub goarch: Option<Vec<String>>,   // arch filter
}
```

Add a custom deserializer similar to `deserialize_signs` (accepts single object or array).

- [ ] **Step 2: Implement UPX in build stage**

After the build loop, add UPX compression:
```rust
// UPX compression
for upx_config in &ctx.config.upx {
    if !upx_config.enabled.unwrap_or(true) { continue; }

    let upx_bin = upx_config.binary.as_deref().unwrap_or("upx");

    // Check if upx is available
    if std::process::Command::new(upx_bin).arg("--version").output().is_err() {
        eprintln!("[upx] {} not found, skipping compression", upx_bin);
        continue;
    }

    let binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
    for artifact in binaries {
        // Apply OS/arch filters
        if let Some(target) = &artifact.target {
            if let Some(goos) = &upx_config.goos {
                let os = target_to_os(target);
                if !goos.iter().any(|g| g == &os) { continue; }
            }
            if let Some(goarch) = &upx_config.goarch {
                let arch = target_to_arch(target);
                if !goarch.iter().any(|g| g == &arch) { continue; }
            }
        }

        let mut cmd = std::process::Command::new(upx_bin);
        if let Some(compress) = &upx_config.compress {
            match compress.as_str() {
                "best" => { cmd.arg("--best"); }
                level => { cmd.arg(format!("-{}", level)); }
            }
        }
        if upx_config.lzma.unwrap_or(false) { cmd.arg("--lzma"); }
        if upx_config.brute.unwrap_or(false) { cmd.arg("--brute"); }
        cmd.arg(&artifact.path);

        if ctx.is_dry_run() {
            eprintln!("[upx] (dry-run) would compress {}", artifact.path.display());
            continue;
        }

        let status = cmd.status().context("failed to run upx")?;
        if !status.success() {
            eprintln!("[upx] warning: upx failed for {}", artifact.path.display());
        } else {
            eprintln!("[upx] compressed {}", artifact.path.display());
        }
    }
}
```

- [ ] **Step 3: Add tests and commit**

```rust
#[test]
fn test_upx_config_parsing() {
    let yaml = r#"
project_name: test
upx:
  - compress: best
    lzma: true
    goos: ["linux", "windows"]
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.upx.len(), 1);
    assert_eq!(config.upx[0].compress.as_deref(), Some("best"));
    assert_eq!(config.upx[0].lzma, Some(true));
}
```

```bash
git add -A && git commit -m "feat: add UPX binary compression stage"
```

---

## Task 5L: Additional Announce Providers

**Files:**
- Create: `crates/stage-announce/src/telegram.rs`
- Create: `crates/stage-announce/src/teams.rs`
- Create: `crates/stage-announce/src/mattermost.rs`
- Create: `crates/stage-announce/src/email.rs`
- Modify: `crates/stage-announce/src/lib.rs` — wire new providers
- Modify: `crates/core/src/config.rs` — add provider configs to `AnnounceConfig`

- [ ] **Step 1: Add config structs**

Extend `AnnounceConfig`:
```rust
pub struct AnnounceConfig {
    pub discord: Option<AnnounceProviderConfig>,
    pub slack: Option<AnnounceProviderConfig>,
    pub webhook: Option<WebhookConfig>,
    pub telegram: Option<TelegramConfig>,
    pub teams: Option<TeamsConfig>,
    pub mattermost: Option<MattermostConfig>,
    pub email: Option<EmailConfig>,
}
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: Option<bool>,
    pub chat_id: Option<String>,
    pub message_template: Option<String>,
    // Token from TELEGRAM_TOKEN env var
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TeamsConfig {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub title: Option<String>,
    pub message_template: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MattermostConfig {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub channel: Option<String>,
    pub username: Option<String>,
    pub icon_url: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EmailConfig {
    pub enabled: Option<bool>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub from: Option<String>,
    pub to: Option<Vec<String>>,
    pub subject_template: Option<String>,
    pub body_template: Option<String>,
    // SMTP credentials from SMTP_USERNAME/SMTP_PASSWORD env vars
}
```

- [ ] **Step 2: Create provider modules**

Each follows the same pattern as discord/slack. Example for Telegram:

`crates/stage-announce/src/telegram.rs`:
```rust
use anyhow::{Context, Result};

pub fn send_telegram(chat_id: &str, message: &str) -> Result<()> {
    let token = std::env::var("TELEGRAM_TOKEN")
        .context("TELEGRAM_TOKEN env var not set")?;

    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
        "parse_mode": "Markdown",
    });

    let client = reqwest::blocking::Client::new();
    let resp = client.post(&url).json(&body).send()
        .context("failed to send Telegram message")?;

    if !resp.status().is_success() {
        anyhow::bail!("Telegram API returned {}", resp.status());
    }

    Ok(())
}
```

Similar implementations for teams (POST to webhook with MessageCard JSON), mattermost (POST to webhook with payload JSON), and email (basic SMTP via lettre or raw TCP, keeping deps minimal — use a simple SMTP implementation or skip if no SMTP crate desired).

- [ ] **Step 3: Wire into announce stage**

In `lib.rs`, add the same pattern used for discord/slack for each new provider:
```rust
// Telegram
if let Some(tg_cfg) = &announce.telegram
    && tg_cfg.enabled.unwrap_or(false)
{
    let chat_id = tg_cfg.chat_id.as_deref()
        .ok_or_else(|| anyhow::anyhow!("announce.telegram: missing chat_id"))?;
    let rendered_chat_id = ctx.render_template(chat_id)?;
    let tmpl = tg_cfg.message_template.as_deref()
        .unwrap_or("{{ .ProjectName }} {{ .Tag }} released!");
    let message = ctx.render_template(tmpl)?;

    if ctx.is_dry_run() {
        eprintln!("[announce] (dry-run) telegram: {}", message);
    } else {
        eprintln!("[announce] telegram: {}", message);
        telegram::send_telegram(&rendered_chat_id, &message)?;
    }
}
```

- [ ] **Step 4: Add tests**

Same pattern as existing announce tests — test disabled skip, dry-run, missing config errors:
```rust
#[test]
fn test_skips_disabled_telegram() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramConfig {
            enabled: Some(false),
            chat_id: Some("-100123456".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_telegram_does_not_send() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramConfig {
            enabled: Some(true),
            chat_id: Some("-100123456".to_string()),
            message_template: Some("{{ .ProjectName }} released!".to_string()),
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions { dry_run: true, ..Default::default() };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}
```

Repeat for teams, mattermost, email.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: add Telegram, Teams, Mattermost, and email announce providers"
```

---

## Task 5M: CLI + Config Additions

**Files:**
- Modify: `crates/cli/src/main.rs` — add `Jsonschema` command
- Modify: `crates/core/src/config.rs` — add `env_files`, `version`, build `ignore`/`overrides`
- Modify: `crates/cli/src/pipeline.rs` — load .env files
- Modify: `crates/stage-build/src/lib.rs` — handle ignore and overrides

### 5M.1: jsonschema command

- [ ] **Step 1: Add `Jsonschema` command**

Add `schemars` dependency: `cargo add schemars --package anodize-core`

Derive `JsonSchema` on all config structs (add `#[derive(schemars::JsonSchema)]` alongside existing derives).

Add command:
```rust
/// Output JSON Schema for config file (IDE autocompletion support)
Jsonschema,
```

Handler:
```rust
Commands::Jsonschema => {
    let schema = schemars::schema_for!(anodize_core::config::Config);
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}
```

### 5M.2: .env file loading

- [ ] **Step 2: Add config field**

In `Config`:
```rust
pub env_files: Option<Vec<String>>,
```

- [ ] **Step 3: Load .env files in pipeline setup**

In `pipeline.rs`, after loading config but before template expansion:
```rust
if let Some(env_files) = &config.env_files {
    for env_file in env_files {
        let path = std::path::Path::new(env_file);
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read env file: {}", env_file))?;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                if let Some((key, value)) = line.split_once('=') {
                    std::env::set_var(key.trim(), value.trim().trim_matches('"'));
                }
            }
        }
    }
}
```

### 5M.3: Config versioning

- [ ] **Step 4: Add version field**

In `Config`:
```rust
pub version: Option<u32>,  // schema version, default 1
```

In config loading, validate:
```rust
if let Some(v) = config.version {
    if v > 2 {
        anyhow::bail!("unsupported config version: {} (supported: 1, 2)", v);
    }
}
```

### 5M.4: Build ignore and overrides

- [ ] **Step 5: Add to BuildConfig**

```rust
pub ignore: Option<Vec<BuildIgnoreRule>>,
pub overrides: Option<Vec<BuildOverrideRule>>,
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildIgnoreRule {
    pub os: Option<String>,
    pub arch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildOverrideRule {
    pub targets: Vec<String>,           // glob patterns for target triples
    pub env: Option<HashMap<String, String>>,
    pub flags: Option<String>,
    pub features: Option<Vec<String>>,
}
```

- [ ] **Step 6: Apply ignore/overrides in build stage**

Before building a target, check if it matches any ignore rule:
```rust
if let Some(ignores) = &build_config.ignore {
    let os = target_to_os(target);
    let arch = target_to_arch(target);
    if ignores.iter().any(|rule| {
        rule.os.as_deref().map(|o| o == os).unwrap_or(true)
            && rule.arch.as_deref().map(|a| a == arch).unwrap_or(true)
    }) {
        eprintln!("[build] ignoring target {} (matched ignore rule)", target);
        continue;
    }
}
```

For overrides, apply matching rules:
```rust
if let Some(overrides) = &build_config.overrides {
    for rule in overrides {
        if rule.targets.iter().any(|pattern| {
            glob::Pattern::new(pattern).map(|p| p.matches(target)).unwrap_or(false)
        }) {
            // Merge override env, flags, features
            if let Some(env) = &rule.env {
                for (k, v) in env { cmd.env(k, v); }
            }
            if let Some(flags) = &rule.flags {
                for flag in flags.split_whitespace() { cmd.arg(flag); }
            }
        }
    }
}
```

- [ ] **Step 7: Add tests for all additions**

```rust
#[test]
fn test_env_files_config() {
    let yaml = r#"
project_name: test
env_files: [".env", ".release.env"]
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.env_files.unwrap().len(), 2);
}

#[test]
fn test_config_version_field() {
    let yaml = r#"
project_name: test
version: 2
crates: []
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.version, Some(2));
}

#[test]
fn test_build_ignore_rule() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test
        ignore:
          - os: windows
            arch: arm64
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let ignore = config.crates[0].builds.as_ref().unwrap()[0].ignore.as_ref().unwrap();
    assert_eq!(ignore[0].os.as_deref(), Some("windows"));
}

#[test]
fn test_build_overrides_rule() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test
        overrides:
          - targets: ["x86_64-*"]
            features: ["simd"]
            flags: "--cfg=target_feature_simd"
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    let ov = config.crates[0].builds.as_ref().unwrap()[0].overrides.as_ref().unwrap();
    assert_eq!(ov[0].targets[0], "x86_64-*");
    assert_eq!(ov[0].features.as_ref().unwrap()[0], "simd");
}
```

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "feat: add jsonschema command, .env loading, config versioning, build ignore/overrides"
```

---

## Task 5N: Maintenance

**Files:**
- Modify: `Cargo.toml` (workspace) — replace `serde_yaml` with `serde_yml`
- Modify: all `Cargo.toml` files referencing `serde_yaml`
- Modify: all `.rs` files using `serde_yaml` — update import paths
- Modify: `.anodize.yaml` — exercise new features

- [ ] **Step 1: Check serde_yaml status and decide on replacement**

Run: `cargo tree -p serde_yaml` to check current version and deprecation status.

If `serde_yaml` 0.9 is deprecated, migrate to `serde_yml`:
```bash
# In workspace Cargo.toml, replace:
# serde_yaml = "0.9" → serde_yml = "0.0.12" (or latest)
```

If `serde_yml` API is compatible, it's a find-and-replace of `serde_yaml` → `serde_yml` across all files. If not, adapt the API calls.

- [ ] **Step 2: Update all imports**

In every `.rs` file that uses `serde_yaml`:
```rust
// Before:
use serde_yaml;
// After:
use serde_yml as serde_yaml; // alias for minimal diff, or rename all callsites
```

Alternative: keep using `serde_yaml` if it's not yet causing issues and the deprecation is only a warning. Check `cargo clippy` output.

- [ ] **Step 3: Update dependencies**

```bash
cargo update
```

Check for any outdated critical deps and update:
```bash
cargo outdated --workspace
```

- [ ] **Step 4: Run full test suite**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check
```

- [ ] **Step 5: Update dogfood config**

Update `.anodize.yaml` to exercise at least one new Session 5 feature (e.g., source archive, SBOM, env_files).

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "chore: migrate serde_yaml to serde_yml, update dependencies"
```

---

## Task 5O: Documentation Site

**Files:**
- Create: `docs/book/` — mdBook source
- Create: `docs/book/book.toml` — mdBook config
- Create: `docs/book/src/SUMMARY.md` — table of contents
- Create: `docs/book/src/` — chapter markdown files

- [ ] **Step 1: Install mdBook**

```bash
cargo install mdbook 2>/dev/null || true
```

- [ ] **Step 2: Create book structure**

Create `docs/book/book.toml`:
```toml
[book]
title = "Anodize Documentation"
authors = ["tj-smith47"]
language = "en"
multilingual = false
src = "src"

[build]
build-dir = "../../dist/docs"

[output.html]
default-theme = "rust"
git-repository-url = "https://github.com/tj-smith47/anodize"
```

Create `docs/book/src/SUMMARY.md`:
```markdown
# Summary

[Introduction](./introduction.md)

# User Guide

- [Getting Started](./getting-started.md)
- [Configuration Reference](./configuration.md)
- [Template Reference](./templates.md)
- [CLI Reference](./cli.md)

# Stages

- [Build](./stages/build.md)
- [Archive](./stages/archive.md)
- [Checksum](./stages/checksum.md)
- [Changelog](./stages/changelog.md)
- [Release](./stages/release.md)
- [Publish](./stages/publish.md)
- [Docker](./stages/docker.md)
- [Sign](./stages/sign.md)
- [Announce](./stages/announce.md)
- [NFpm](./stages/nfpm.md)
- [Source & SBOM](./stages/source-sbom.md)
- [UPX Compression](./stages/upx.md)

# CI/CD Integration

- [GitHub Actions](./ci/github-actions.md)
- [GitLab CI](./ci/gitlab-ci.md)

# Advanced

- [Auto-Tagging](./advanced/auto-tagging.md)
- [Nightly Builds](./advanced/nightly.md)
- [Config Includes](./advanced/config-includes.md)
- [Monorepo Support](./advanced/monorepo.md)
- [Reproducible Builds](./advanced/reproducible.md)
- [Universal Binaries](./advanced/universal-binaries.md)

# Migration

- [From GoReleaser](./migration/goreleaser.md)
- [From cargo-dist](./migration/cargo-dist.md)

# FAQ

- [FAQ](./faq.md)
```

- [ ] **Step 3: Create chapter files**

Create each `.md` file with content drawn from existing `docs/configuration.md`, `docs/templates.md`, and the design spec. Key chapters:

`src/introduction.md` — What is anodize, why use it, quick feature overview.

`src/getting-started.md` — Install, `anodize init`, first release.

`src/configuration.md` — Full config reference (copy from existing docs/configuration.md and expand).

`src/templates.md` — Template variable reference (copy from existing docs/templates.md).

`src/cli.md` — Every command and flag with examples.

Stage chapters — one per stage, explaining config fields and examples.

- [ ] **Step 4: Build and verify**

```bash
cd /opt/repos/anodize/docs/book && mdbook build
```

Verify the output exists at `dist/docs/index.html`.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "docs: add mdBook documentation site with full reference"
```

---

## Final Verification

After all tasks complete:

- [ ] **Run full test suite**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check
```

- [ ] **Count tests**

```bash
cargo test --workspace 2>&1 | grep "test result"
```

Target: 900+ tests (812 baseline + ~100 new from Session 5 features).

- [ ] **Final commit if needed**

```bash
git add -A && git commit -m "feat: complete Session 5 — extended features completeness pass"
```
