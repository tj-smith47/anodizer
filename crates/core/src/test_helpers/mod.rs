//! Shared test infrastructure for the anodizer workspace.
//!
//! This module is gated behind the `test-helpers` feature so that other crates
//! in the workspace can pull it in as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! anodizer-core = { workspace = true, features = ["test-helpers"] }
//! ```
//!
//! Provides:
//! - [`TestContextBuilder`] — fluent builder for [`Context`] with sensible defaults
//! - [`Context::test_fixture`] - stable minimally-populated Context for cross-crate unit tests
//! - [`CwdGuard`] — RAII helper that restores the original cwd on Drop (panic-safe)
//! - [`create_test_project`] — creates a minimal Cargo project on disk
//! - [`init_git_repo`] — initializes a git repo with config, initial commit, and tag
//! - [`init_git_repo_with_commits`] — initializes a git repo with multiple commits
//! - [`create_config`] — writes `.anodizer.yaml`
//! - [`make_git_info`] — creates a [`GitInfo`] with sensible defaults
//! - [`create_fake_binary`] — creates a dummy binary file for archive/checksum tests
//! - [`responder`] — shared in-process HTTP responder for unit tests
//!   (consolidates ~11 inline copies; fixes the v0.3.0 chocolatey /
//!   v0.3.0 github-rate-limit CI flakes)

pub mod artifact_set;
pub mod env;
pub mod https_responder;
pub mod responder;
pub mod scripted_responder;

use crate::config::{Config, CrateConfig, Defaults, SignConfig, UpxConfig, WorkspaceConfig};
use crate::context::{Context, ContextOptions};
use crate::git::{GitInfo, SemVer};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// CwdGuard — panic-safe cwd restore
// ---------------------------------------------------------------------------

/// RAII guard that captures the current working directory on construction,
/// switches to `target`, and restores the original cwd on Drop.
///
/// This makes cwd-mutating tests panic-safe: if the test body panics between
/// `CwdGuard::new(...)` and the end of the scope, the original cwd is still
/// restored when the guard unwinds — preventing one test's failure from
/// contaminating subsequent tests in the same process.
///
/// Pair with `#[serial]` (from the `serial_test` crate) when multiple tests
/// in a file mutate cwd, since changing cwd is a process-wide side effect.
/// The whole crate's cwd-touching tests must share ONE serial key so they
/// mutually exclude: process-global cwd is also read by tests that spawn a
/// cwd-sensitive subprocess (e.g. `rustc -vV` in `partial.rs`), which inherit
/// the cwd and fail spuriously if a concurrent test moved it. Use the default
/// (unnamed) `#[serial]` key everywhere — a distinct keyed group would run in
/// parallel and reopen the race.
///
/// # Example
///
/// ```rust,ignore
/// use anodizer_core::test_helpers::CwdGuard;
///
/// #[test]
/// #[serial]
/// fn my_test() {
///     let tmp = tempfile::tempdir().unwrap();
///     let _guard = CwdGuard::new(tmp.path()).unwrap();
///     // ... test body; cwd is now tmp.path() ...
///     // panic-safe: original cwd is restored when `_guard` drops.
/// }
/// ```
pub struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    /// Capture the current cwd and switch to `target`. Returns the guard;
    /// the original cwd is restored when the guard is dropped.
    pub fn new(target: impl AsRef<Path>) -> std::io::Result<Self> {
        let original = std::env::current_dir()?;
        std::env::set_current_dir(target.as_ref())?;
        Ok(Self { original })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best-effort: ignore the error during unwind so we never double-panic.
        let _ = std::env::set_current_dir(&self.original);
    }
}

// ---------------------------------------------------------------------------
// TestContextBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for [`Context`] with sensible defaults suitable for tests.
///
/// # Example
///
/// ```rust,ignore
/// use anodizer_core::test_helpers::TestContextBuilder;
///
/// let ctx = TestContextBuilder::new()
///     .project_name("my-app")
///     .tag("v2.0.0")
///     .dry_run(true)
///     .build();
///
/// assert_eq!(ctx.template_vars().get("ProjectName").map(|s| s.as_str()), Some("my-app"));
/// assert_eq!(ctx.template_vars().get("Tag").map(|s| s.as_str()), Some("v2.0.0"));
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
    workspaces: Option<Vec<WorkspaceConfig>>,
    populate_git_vars: bool,
    dist: Option<PathBuf>,
    signs: Vec<SignConfig>,
    binary_signs: Vec<SignConfig>,
    upx: Vec<UpxConfig>,
    defaults: Option<Defaults>,
    source: Option<crate::config::SourceConfig>,
    sboms: Vec<crate::config::SbomConfig>,
    project_root: Option<PathBuf>,
    env_overrides: Vec<(String, String)>,
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
                build_metadata: None,
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
            workspaces: None,
            populate_git_vars: true,
            dist: None,
            signs: Vec::new(),
            binary_signs: Vec::new(),
            upx: Vec::new(),
            defaults: None,
            source: None,
            sboms: Vec::new(),
            project_root: None,
            env_overrides: Vec::new(),
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
        if let Ok(sv) = crate::git::parse_semver_tag(tag) {
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

    /// Add workspace configurations to the config.
    pub fn workspaces(mut self, workspaces: Vec<WorkspaceConfig>) -> Self {
        self.workspaces = Some(workspaces);
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

    /// Set binary-specific sign configurations.
    pub fn binary_signs(mut self, binary_signs: Vec<SignConfig>) -> Self {
        self.binary_signs = binary_signs;
        self
    }

    /// Set UPX configurations.
    pub fn upx(mut self, upx: Vec<UpxConfig>) -> Self {
        self.upx = upx;
        self
    }

    /// Set default configuration (e.g. global checksum skip).
    pub fn defaults(mut self, defaults: Defaults) -> Self {
        self.defaults = Some(defaults);
        self
    }

    /// Set source archive configuration.
    pub fn source(mut self, source: crate::config::SourceConfig) -> Self {
        self.source = Some(source);
        self
    }

    /// Add an SBOM configuration to the list.
    pub fn add_sbom(mut self, sbom: crate::config::SbomConfig) -> Self {
        self.sboms.push(sbom);
        self
    }

    /// Set explicit project root directory (avoids process-wide CWD mutation in tests).
    pub fn project_root(mut self, root: PathBuf) -> Self {
        self.project_root = Some(root);
        self
    }

    /// Add an environment variable override. Calling [`env`](Self::env)
    /// at least once swaps the built context's env source to a
    /// [`MapEnvSource`](crate::MapEnvSource) seeded from the
    /// accumulated overrides — so production code that reads through
    /// [`Context::env_var`](crate::context::Context::env_var) sees the
    /// injected values without `std::env::set_var`. Calls accumulate
    /// (later wins on duplicate key).
    pub fn env<K: Into<String>, V: Into<String>>(mut self, k: K, v: V) -> Self {
        self.env_overrides.push((k.into(), v.into()));
        self
    }

    /// Build the [`Context`] with the configured values.
    #[allow(clippy::field_reassign_with_default)]
    pub fn build(self) -> Context {
        let mut config = Config::default();
        config.project_name = self.project_name;
        config.crates = self.crates;
        config.workspaces = self.workspaces;
        config.signs = self.signs;
        config.binary_signs = self.binary_signs;
        config.upx = self.upx;
        config.defaults = self.defaults;
        config.source = self.source;
        config.sboms = self.sboms;
        if let Some(dist) = self.dist {
            config.dist = dist;
        }

        let options = ContextOptions {
            snapshot: self.snapshot,
            nightly: false,
            dry_run: self.dry_run,
            quiet: false,
            verbose: self.verbose,
            debug: self.debug,
            skip_stages: self.skip_stages,
            selected_crates: self.selected_crates,
            token: self.token,
            parallelism: self.parallelism,
            single_target: self.single_target,
            release_notes_path: None,
            fail_fast: false,
            partial_target: None,
            merge: false,
            publish_only: false,
            project_root: self.project_root,
            strict: false,
            resume_release: false,
            replace_existing_artifacts: false,
            skip_post_publish_poll: false,
            gate_submitter: None,
            rollback_mode: None,
            simulate_failure_publishers: Vec::new(),
            rollback_only: false,
            allow_rerun: false,
            from_run: None,
            runtime_nondeterministic_allowlist: Vec::new(),
            summary_json_path: None,
            allow_ai_failure: false,
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
            remote_url: String::new(),
            summary: String::new(),
            tag_subject: String::new(),
            tag_contents: String::new(),
            tag_body: String::new(),
            first_commit: None,
        });

        if self.populate_git_vars {
            ctx.populate_git_vars();
        }

        ctx.populate_metadata_var().unwrap();

        if !self.env_overrides.is_empty() {
            let mut src = crate::MapEnvSource::new();
            for (k, v) in self.env_overrides {
                src.set(k, v);
            }
            ctx.set_env_source(src);
        }

        ctx
    }
}

// ---------------------------------------------------------------------------
// Context::test_fixture — stable entry point for downstream unit tests
// ---------------------------------------------------------------------------

impl Context {
    /// Return a minimally-populated [`Context`] suitable for unit tests in
    /// downstream crates.
    ///
    /// This is a thin wrapper over [`TestContextBuilder`] with a fixed tag
    /// (`v0.0.0-test`) so the value is stable and obviously synthetic across
    /// every crate that builds a [`Context`] in `#[cfg(test)]` code. Use the
    /// builder directly when a test needs to vary fields.
    ///
    /// Gated by the `test-helpers` feature; enable it in your crate's
    /// `[dev-dependencies]` to access this constructor:
    ///
    /// ```toml
    /// [dev-dependencies]
    /// anodizer-core = { workspace = true, features = ["test-helpers"] }
    /// ```
    pub fn test_fixture() -> Context {
        TestContextBuilder::new().tag("v0.0.0-test").build()
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
    .unwrap_or_else(|e| panic!("failed to write Cargo.toml: {e}"));

    fs::create_dir_all(dir.join("src")).unwrap_or_else(|e| panic!("failed to create src/: {e}"));
    fs::write(
        dir.join("src/main.rs"),
        r#"fn main() { println!("hello"); }"#,
    )
    .unwrap_or_else(|e| panic!("failed to write src/main.rs: {e}"));
}

/// Write an `.anodizer.yaml` config file in the given directory.
pub fn create_config(dir: &Path, content: &str) {
    fs::write(dir.join(".anodizer.yaml"), content)
        .unwrap_or_else(|e| panic!("failed to write .anodizer.yaml: {e}"));
}

/// Create a fake binary file at `dir/<name>` for testing archive/checksum stages.
///
/// The file contains a small amount of recognizable data so tests can
/// verify the file was included in archives or checksum outputs.
pub fn create_fake_binary(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create parent dir for fake binary: {e}"));
    }
    // Write a recognizable pattern that is not all zeros
    let data: Vec<u8> = (0..256u16).map(|i| (i % 256) as u8).collect();
    fs::write(&path, &data).unwrap_or_else(|e| panic!("failed to write fake binary: {e}"));
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
        .unwrap_or_else(|e| panic!("git command failed to spawn: {e}"));
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
            .unwrap_or_else(|e| panic!("failed to write commit file: {e}"));
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
            build_metadata: None,
        },
        commit_date: "2026-03-25T10:30:00+00:00".to_string(),
        commit_timestamp: "1774463400".to_string(),
        previous_tag: Some("v1.2.2".to_string()),
        remote_url: String::new(),
        summary: String::new(),
        tag_subject: String::new(),
        tag_contents: String::new(),
        tag_body: String::new(),
        first_commit: None,
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
        assert_eq!(ctx.template_vars().get("Tag"), Some(&"v1.2.3".to_string()));
        assert_eq!(
            ctx.template_vars().get("Version"),
            Some(&"1.2.3".to_string())
        );
        assert_eq!(ctx.template_vars().get("Major"), Some(&"1".to_string()));
        assert_eq!(ctx.template_vars().get("Branch"), Some(&"main".to_string()));
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
        assert_eq!(ctx.template_vars().get("Major"), Some(&"3".to_string()));
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
        let ctx = TestContextBuilder::new().populate_git_vars(false).build();
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
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        assert_eq!(ctx.config.crates.len(), 1);
        assert_eq!(ctx.config.crates[0].name, "my-crate");
    }

    #[test]
    fn context_test_fixture_builds_without_panic() {
        let ctx = Context::test_fixture();
        // Tag is the documented stable value so downstream crates can
        // assert against it.
        assert_eq!(
            ctx.template_vars().get("Tag"),
            Some(&"v0.0.0-test".to_string())
        );
        let info = ctx
            .git_info
            .as_ref()
            .expect("test_fixture must populate git_info");
        assert_eq!(info.tag, "v0.0.0-test");
        assert_eq!(info.semver.major, 0);
        assert_eq!(info.semver.minor, 0);
        assert_eq!(info.semver.patch, 0);
        assert_eq!(info.semver.prerelease.as_deref(), Some("test"));
    }

    #[test]
    fn test_context_builder_env_injects_map_source() {
        let ctx = TestContextBuilder::new()
            .env("A", "1")
            .env("B", "2")
            .build();
        assert_eq!(ctx.env_var("A"), Some("1".to_string()));
        assert_eq!(ctx.env_var("B"), Some("2".to_string()));
        assert_eq!(ctx.env_var("C"), None);
    }

    #[test]
    fn test_context_builder_without_env_uses_process_source() {
        let ctx = TestContextBuilder::new().build();
        // An unset name returns None regardless of which source is wired.
        // The assertion targets the unset-var branch so the test stays
        // deterministic across CI / dev shells.
        assert_eq!(ctx.env_var("ANODIZER_T3_UNSET_VAR_X"), None);
    }
}
