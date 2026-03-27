//! Shared test infrastructure for the anodize workspace.
//!
//! This module is gated behind the `test-helpers` feature so that other crates
//! in the workspace can pull it in as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! anodize-core = { workspace = true, features = ["test-helpers"] }
//! ```
//!
//! Provides:
//! - [`TestContextBuilder`] — fluent builder for [`Context`] with sensible defaults
//! - [`create_test_project`] — creates a minimal Cargo project on disk
//! - [`init_git_repo`] — initializes a git repo with config, initial commit, and tag
//! - [`init_git_repo_with_commits`] — initializes a git repo with multiple commits
//! - [`create_config`] — writes `.anodize.yaml`
//! - [`make_git_info`] — creates a [`GitInfo`] with sensible defaults
//! - [`create_fake_binary`] — creates a dummy binary file for archive/checksum tests

use crate::config::{Config, CrateConfig, Defaults, SignConfig};
use crate::context::{Context, ContextOptions};
use crate::git::{GitInfo, SemVer};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// TestContextBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for [`Context`] with sensible defaults suitable for tests.
///
/// # Example
///
/// ```rust,ignore
/// use anodize_core::test_helpers::TestContextBuilder;
///
/// let ctx = TestContextBuilder::new()
///     .project_name("my-app")
///     .tag("v2.0.0")
///     .dry_run(true)
///     .build();
///
/// assert_eq!(ctx.template_vars().get("ProjectName").unwrap(), "my-app");
/// assert_eq!(ctx.template_vars().get("Tag").unwrap(), "v2.0.0");
/// ```
pub struct TestContextBuilder {
    project_name: String,
    tag: String,
    commit: String,
    short_commit: String,
    branch: String,
    dirty: bool,
    semver: SemVer,
    commit_date: String,
    commit_timestamp: String,
    previous_tag: Option<String>,
    snapshot: bool,
    dry_run: bool,
    verbose: bool,
    debug: bool,
    skip_stages: Vec<String>,
    selected_crates: Vec<String>,
    token: Option<String>,
    parallelism: usize,
    single_target: Option<String>,
    crates: Vec<CrateConfig>,
    populate_git_vars: bool,
    dist: Option<PathBuf>,
    signs: Vec<SignConfig>,
    defaults: Option<Defaults>,
    source: Option<crate::config::SourceConfig>,
    sbom: Option<crate::config::SbomConfig>,
}

impl Default for TestContextBuilder {
    fn default() -> Self {
        Self {
            project_name: "test-project".to_string(),
            tag: "v1.2.3".to_string(),
            commit: "abc123def456abc123def456abc123def456abc1".to_string(),
            short_commit: "abc123d".to_string(),
            branch: "main".to_string(),
            dirty: false,
            semver: SemVer {
                major: 1,
                minor: 2,
                patch: 3,
                prerelease: None,
            },
            commit_date: "2026-03-25T10:30:00+00:00".to_string(),
            commit_timestamp: "1774463400".to_string(),
            previous_tag: Some("v1.2.2".to_string()),
            snapshot: false,
            dry_run: false,
            verbose: false,
            debug: false,
            skip_stages: Vec::new(),
            selected_crates: Vec::new(),
            token: None,
            parallelism: 1,
            single_target: None,
            crates: Vec::new(),
            populate_git_vars: true,
            dist: None,
            signs: Vec::new(),
            defaults: None,
            source: None,
            sbom: None,
        }
    }
}

impl TestContextBuilder {
    /// Create a new builder with sensible defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the project name (populates `Config::project_name` and the `ProjectName` template var).
    pub fn project_name(mut self, name: &str) -> Self {
        self.project_name = name.to_string();
        self
    }

    /// Set the git tag. Also parses it to update the semver fields.
    /// If parsing fails, only the tag string is updated.
    pub fn tag(mut self, tag: &str) -> Self {
        self.tag = tag.to_string();
        if let Ok(sv) = crate::git::parse_semver(tag) {
            self.semver = sv;
        }
        self
    }

    /// Set the full commit SHA.
    pub fn commit(mut self, commit: &str) -> Self {
        self.commit = commit.to_string();
        self.short_commit = commit.chars().take(7).collect();
        self
    }

    /// Set the git branch.
    pub fn branch(mut self, branch: &str) -> Self {
        self.branch = branch.to_string();
        self
    }

    /// Set the dirty flag.
    pub fn dirty(mut self, dirty: bool) -> Self {
        self.dirty = dirty;
        self
    }

    /// Set a prerelease suffix (e.g. "rc.1", "beta.2").
    pub fn prerelease(mut self, pre: Option<&str>) -> Self {
        self.semver.prerelease = pre.map(|s| s.to_string());
        self
    }

    /// Set the previous tag.
    pub fn previous_tag(mut self, tag: Option<&str>) -> Self {
        self.previous_tag = tag.map(|s| s.to_string());
        self
    }

    /// Enable or disable snapshot mode.
    pub fn snapshot(mut self, snapshot: bool) -> Self {
        self.snapshot = snapshot;
        self
    }

    /// Enable or disable dry-run mode.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Enable or disable verbose mode.
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Enable or disable debug mode.
    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Set stages to skip.
    pub fn skip_stages(mut self, stages: Vec<String>) -> Self {
        self.skip_stages = stages;
        self
    }

    /// Set selected crates filter.
    pub fn selected_crates(mut self, crates: Vec<String>) -> Self {
        self.selected_crates = crates;
        self
    }

    /// Set the GitHub token.
    pub fn token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Set the parallelism level.
    pub fn parallelism(mut self, p: usize) -> Self {
        self.parallelism = p;
        self
    }

    /// Set a single target triple.
    pub fn single_target(mut self, target: Option<String>) -> Self {
        self.single_target = target;
        self
    }

    /// Add crate configurations to the config.
    pub fn crates(mut self, crates: Vec<CrateConfig>) -> Self {
        self.crates = crates;
        self
    }

    /// Whether to auto-populate git template vars (default: true).
    /// Set to false if you want to test the context before git vars are populated.
    pub fn populate_git_vars(mut self, populate: bool) -> Self {
        self.populate_git_vars = populate;
        self
    }

    /// Set the dist directory (output directory for artifacts).
    pub fn dist(mut self, dist: PathBuf) -> Self {
        self.dist = Some(dist);
        self
    }

    /// Set sign configurations.
    pub fn signs(mut self, signs: Vec<SignConfig>) -> Self {
        self.signs = signs;
        self
    }

    /// Set default configuration (e.g. global checksum disable).
    pub fn defaults(mut self, defaults: Defaults) -> Self {
        self.defaults = Some(defaults);
        self
    }

    /// Set source archive configuration.
    pub fn source(mut self, source: crate::config::SourceConfig) -> Self {
        self.source = Some(source);
        self
    }

    /// Set SBOM configuration.
    pub fn sbom(mut self, sbom: crate::config::SbomConfig) -> Self {
        self.sbom = Some(sbom);
        self
    }

    /// Build the [`Context`] with the configured values.
    #[allow(clippy::field_reassign_with_default)]
    pub fn build(self) -> Context {
        let mut config = Config::default();
        config.project_name = self.project_name;
        config.crates = self.crates;
        config.signs = self.signs;
        config.defaults = self.defaults;
        config.source = self.source;
        config.sbom = self.sbom;
        if let Some(dist) = self.dist {
            config.dist = dist;
        }

        let options = ContextOptions {
            snapshot: self.snapshot,
            nightly: false,
            dry_run: self.dry_run,
            verbose: self.verbose,
            debug: self.debug,
            skip_stages: self.skip_stages,
            selected_crates: self.selected_crates,
            token: self.token,
            parallelism: self.parallelism,
            single_target: self.single_target,
            release_notes_path: None,
        };

        let mut ctx = Context::new(config, options);

        ctx.git_info = Some(GitInfo {
            tag: self.tag,
            commit: self.commit,
            short_commit: self.short_commit,
            branch: self.branch,
            dirty: self.dirty,
            semver: self.semver,
            commit_date: self.commit_date,
            commit_timestamp: self.commit_timestamp,
            previous_tag: self.previous_tag,
        });

        if self.populate_git_vars {
            ctx.populate_git_vars();
        }

        ctx
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Create a minimal Cargo project in the given directory.
///
/// Writes a `Cargo.toml` (with project name "test-project") and `src/main.rs`.
pub fn create_test_project(dir: &Path) {
    fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "test-project"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "test-project"
path = "src/main.rs"
"#,
    )
    .expect("failed to write Cargo.toml");

    fs::create_dir_all(dir.join("src")).expect("failed to create src/");
    fs::write(
        dir.join("src/main.rs"),
        r#"fn main() { println!("hello"); }"#,
    )
    .expect("failed to write src/main.rs");
}

/// Write an `.anodize.yaml` config file in the given directory.
pub fn create_config(dir: &Path, content: &str) {
    fs::write(dir.join(".anodize.yaml"), content).expect("failed to write .anodize.yaml");
}

/// Create a fake binary file at `dir/<name>` for testing archive/checksum stages.
///
/// The file contains a small amount of recognizable data so tests can
/// verify the file was included in archives or checksum outputs.
pub fn create_fake_binary(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create parent dir for fake binary");
    }
    // Write a recognizable pattern that is not all zeros
    let data: Vec<u8> = (0..256u16).map(|i| (i % 256) as u8).collect();
    fs::write(&path, &data).expect("failed to write fake binary");
    path
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Run a git command in `dir`, panicking on failure.
fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git command failed to spawn");
    assert!(
        output.status.success(),
        "git {:?} failed with status {}: {}",
        args,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialize a git repository with a config, initial commit, and `v0.1.0` tag.
///
/// Expects that the directory already has files to commit (e.g. from [`create_test_project`]).
pub fn init_git_repo(dir: &Path) {
    run_git(dir, &["init"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "initial"]);
    run_git(dir, &["tag", "v0.1.0"]);
}

/// Initialize a git repository with multiple commits.
///
/// Each entry in `commits` becomes a separate commit. A file `commit_N.txt` is
/// created for each so that there is always something to commit. A `v0.1.0` tag
/// is placed on the first commit.
pub fn init_git_repo_with_commits(dir: &Path, commits: &[&str]) {
    run_git(dir, &["init"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);

    for (i, message) in commits.iter().enumerate() {
        let filename = format!("commit_{}.txt", i);
        fs::write(dir.join(&filename), format!("content for commit {}", i))
            .expect("failed to write commit file");
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-m", message]);

        // Tag the first commit
        if i == 0 {
            run_git(dir, &["tag", "v0.1.0"]);
        }
    }
}

// ---------------------------------------------------------------------------
// GitInfo helper
// ---------------------------------------------------------------------------

/// Create a [`GitInfo`] with sensible defaults for testing.
///
/// - tag: `v1.2.3`, semver: 1.2.3
/// - commit: `abc123def456...`
/// - branch: `main`
/// - previous_tag: `v1.2.2`
///
/// The `dirty` and `prerelease` parameters control the most commonly varied fields.
pub fn make_git_info(dirty: bool, prerelease: Option<&str>) -> GitInfo {
    GitInfo {
        tag: "v1.2.3".to_string(),
        commit: "abc123def456abc123def456abc123def456abc1".to_string(),
        short_commit: "abc123d".to_string(),
        branch: "main".to_string(),
        dirty,
        semver: SemVer {
            major: 1,
            minor: 2,
            patch: 3,
            prerelease: prerelease.map(|s| s.to_string()),
        },
        commit_date: "2026-03-25T10:30:00+00:00".to_string(),
        commit_timestamp: "1774463400".to_string(),
        previous_tag: Some("v1.2.2".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Module-level tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_default_produces_valid_context() {
        let ctx = TestContextBuilder::new().build();
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"test-project".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Tag"),
            Some(&"v1.2.3".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Version"),
            Some(&"1.2.3".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Major"),
            Some(&"1".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Branch"),
            Some(&"main".to_string())
        );
        assert!(!ctx.is_dry_run());
        assert!(!ctx.is_snapshot());
    }

    #[test]
    fn test_builder_custom_project_name() {
        let ctx = TestContextBuilder::new().project_name("my-app").build();
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"my-app".to_string())
        );
    }

    #[test]
    fn test_builder_custom_tag_updates_semver() {
        let ctx = TestContextBuilder::new().tag("v3.0.0-rc.1").build();
        assert_eq!(
            ctx.template_vars().get("Tag"),
            Some(&"v3.0.0-rc.1".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Major"),
            Some(&"3".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Prerelease"),
            Some(&"rc.1".to_string())
        );
    }

    #[test]
    fn test_builder_dirty_flag() {
        let ctx = TestContextBuilder::new().dirty(true).build();
        assert_eq!(
            ctx.template_vars().get("IsGitDirty"),
            Some(&"true".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("GitTreeState"),
            Some(&"dirty".to_string())
        );
    }

    #[test]
    fn test_builder_dry_run() {
        let ctx = TestContextBuilder::new().dry_run(true).build();
        assert!(ctx.is_dry_run());
    }

    #[test]
    fn test_builder_snapshot() {
        let ctx = TestContextBuilder::new().snapshot(true).build();
        assert!(ctx.is_snapshot());
        assert_eq!(
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_builder_skip_stages() {
        let ctx = TestContextBuilder::new()
            .skip_stages(vec!["build".to_string(), "publish".to_string()])
            .build();
        assert!(ctx.should_skip("build"));
        assert!(ctx.should_skip("publish"));
        assert!(!ctx.should_skip("release"));
    }

    #[test]
    fn test_builder_no_populate_git_vars() {
        let ctx = TestContextBuilder::new()
            .populate_git_vars(false)
            .build();
        // ProjectName is set in Context::new, not populate_git_vars
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"test-project".to_string())
        );
        // Tag should not be set since we skipped populate_git_vars
        assert_eq!(ctx.template_vars().get("Tag"), None);
    }

    #[test]
    fn test_make_git_info_clean() {
        let info = make_git_info(false, None);
        assert!(!info.dirty);
        assert_eq!(info.semver.prerelease, None);
        assert_eq!(info.tag, "v1.2.3");
    }

    #[test]
    fn test_make_git_info_dirty_with_prerelease() {
        let info = make_git_info(true, Some("beta.1"));
        assert!(info.dirty);
        assert_eq!(info.semver.prerelease, Some("beta.1".to_string()));
    }

    #[test]
    fn test_create_fake_binary() {
        let tmp = tempfile::TempDir::new().unwrap();

        let path = create_fake_binary(tmp.path(), "myapp");
        assert!(path.exists());
        let data = fs::read(&path).unwrap();
        assert_eq!(data.len(), 256);
    }

    #[test]
    fn test_create_fake_binary_nested() {
        let tmp = tempfile::TempDir::new().unwrap();

        let path = create_fake_binary(tmp.path(), "subdir/myapp");
        assert!(path.exists());
    }

    #[test]
    fn test_builder_render_template() {
        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v2.0.0")
            .build();
        let result = ctx
            .render_template("{{ .ProjectName }}-{{ .Version }}")
            .unwrap();
        assert_eq!(result, "myapp-2.0.0");
    }

    #[test]
    fn test_builder_with_crates() {
        let crate_cfg = CrateConfig {
            name: "my-crate".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![crate_cfg])
            .build();
        assert_eq!(ctx.config.crates.len(), 1);
        assert_eq!(ctx.config.crates[0].name, "my-crate");
    }
}
