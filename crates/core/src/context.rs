use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::git::GitInfo;
use crate::log::{StageLogger, Verbosity};
use crate::partial::PartialTarget;
use crate::template::TemplateVars;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct ContextOptions {
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
    pub skip_stages: Vec<String>,
    pub selected_crates: Vec<String>,
    pub token: Option<String>,
    /// Maximum number of parallel build jobs (minimum 1).
    pub parallelism: usize,
    /// When set, build only for this single host target triple.
    pub single_target: Option<String>,
    /// Path to a custom release notes file (overrides changelog).
    pub release_notes_path: Option<PathBuf>,
    /// Partial build target for split/merge mode. When set, the build stage
    /// filters targets to only those matching this partial target.
    pub partial_target: Option<PartialTarget>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            snapshot: false,
            nightly: false,
            dry_run: false,
            quiet: false,
            verbose: false,
            debug: false,
            skip_stages: Vec::new(),
            selected_crates: Vec::new(),
            token: None,
            parallelism: 1,
            single_target: None,
            release_notes_path: None,
            partial_target: None,
        }
    }
}

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    /// Set by changelog stage when `use: github-native` is configured.
    /// The release stage reads this to set `generate_release_notes(true)` on the GitHub API.
    pub github_native_changelog: bool,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
    pub changelogs: HashMap<String, String>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            github_native_changelog: false,
            template_vars: vars,
            git_info: None,
            changelogs: HashMap::new(),
        }
    }

    pub fn template_vars(&self) -> &TemplateVars {
        &self.template_vars
    }

    pub fn template_vars_mut(&mut self) -> &mut TemplateVars {
        &mut self.template_vars
    }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub fn is_snapshot(&self) -> bool {
        self.options.snapshot
    }

    pub fn is_nightly(&self) -> bool {
        self.options.nightly
    }

    /// Return the current `Version` template variable, or an empty string if
    /// not yet populated.
    pub fn version(&self) -> String {
        self.template_vars
            .get("Version")
            .cloned()
            .unwrap_or_default()
    }

    /// Derive the verbosity level from context options.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_flags(self.options.quiet, self.options.verbose, self.options.debug)
    }

    /// Create a [`StageLogger`] for the given stage name.
    pub fn logger(&self, stage: &'static str) -> StageLogger {
        StageLogger::new(stage, self.verbosity())
    }

    /// Populate template variables from `self.git_info`.
    ///
    /// Must be called after `self.git_info` is set. Sets the following vars:
    /// - `Tag`, `Version`, `RawVersion` — tag and version strings
    /// - `Major`, `Minor`, `Patch` — semver components
    /// - `Prerelease` — prerelease suffix (or empty)
    /// - `FullCommit`, `Commit` — full commit SHA (`Commit` is alias for `FullCommit`)
    /// - `ShortCommit` — abbreviated commit SHA
    /// - `Branch` — current git branch
    /// - `CommitDate` — ISO 8601 author date of HEAD commit
    /// - `CommitTimestamp` — unix timestamp of HEAD commit
    /// - `IsGitDirty` — "true"/"false"
    /// - `IsGitClean` — "true"/"false" (inverse of `IsGitDirty`)
    /// - `GitTreeState` — "clean"/"dirty"
    /// - `GitURL` — git remote URL
    /// - `Summary` — git describe summary
    /// - `TagSubject` — annotated tag subject or commit subject
    /// - `TagContents` — full annotated tag message or commit message
    /// - `TagBody` — tag message body or commit message body
    /// - `IsSnapshot` — from context options
    /// - `IsNightly` — from context options
    /// - `IsDraft` — "false" (stages may override to "true")
    /// - `IsSingleTarget` — "true"/"false" based on single_target option
    /// - `PreviousTag` — previous matching tag (or empty)
    ///
    /// **Stage-scoped variables** (NOT set here; set per-artifact during stage execution):
    /// - `Binary` — binary name, set by build stage per binary and archive stage per archive
    /// - `ArtifactName` — output artifact filename, set by archive stage after creating each archive
    /// - `ArtifactPath` — absolute path to artifact, set by archive stage after creating each archive
    /// - `Os` — target OS, set by archive/nfpm stages per target
    /// - `Arch` — target architecture, set by archive/nfpm stages per target
    pub fn populate_git_vars(&mut self) {
        if let Some(ref info) = self.git_info {
            let version = format!(
                "{}.{}.{}{}",
                info.semver.major,
                info.semver.minor,
                info.semver.patch,
                info.semver
                    .prerelease
                    .as_ref()
                    .map(|p| format!("-{p}"))
                    .unwrap_or_default()
            );

            self.template_vars.set("Tag", &info.tag);
            self.template_vars.set("Version", &version);
            self.template_vars.set("RawVersion", &version);
            self.template_vars
                .set("Major", &info.semver.major.to_string());
            self.template_vars
                .set("Minor", &info.semver.minor.to_string());
            self.template_vars
                .set("Patch", &info.semver.patch.to_string());
            self.template_vars.set(
                "Prerelease",
                info.semver.prerelease.as_deref().unwrap_or(""),
            );
            self.template_vars.set("FullCommit", &info.commit);
            self.template_vars.set("Commit", &info.commit);
            self.template_vars.set("ShortCommit", &info.short_commit);
            self.template_vars.set("Branch", &info.branch);
            self.template_vars.set("CommitDate", &info.commit_date);
            self.template_vars
                .set("CommitTimestamp", &info.commit_timestamp);
            self.template_vars
                .set("IsGitDirty", if info.dirty { "true" } else { "false" });
            self.template_vars
                .set("IsGitClean", if info.dirty { "false" } else { "true" });
            self.template_vars
                .set("GitTreeState", if info.dirty { "dirty" } else { "clean" });
            self.template_vars.set("GitURL", &info.remote_url);
            self.template_vars.set("Summary", &info.summary);
            self.template_vars.set("TagSubject", &info.tag_subject);
            self.template_vars.set("TagContents", &info.tag_contents);
            self.template_vars.set("TagBody", &info.tag_body);
            self.template_vars
                .set("PreviousTag", info.previous_tag.as_deref().unwrap_or(""));
        }

        self.template_vars.set(
            "IsSnapshot",
            if self.options.snapshot {
                "true"
            } else {
                "false"
            },
        );
        self.template_vars.set(
            "IsNightly",
            if self.options.nightly {
                "true"
            } else {
                "false"
            },
        );
        self.template_vars.set("IsDraft", "false");
        self.template_vars.set(
            "IsSingleTarget",
            if self.options.single_target.is_some() {
                "true"
            } else {
                "false"
            },
        );
    }

    /// Populate time-related template variables using the current UTC time.
    ///
    /// Sets:
    /// - `Date` — current date as YYYY-MM-DD
    /// - `Timestamp` — current unix timestamp as string
    /// - `Now` — current UTC time as ISO 8601
    pub fn populate_time_vars(&mut self) {
        let now = Utc::now();
        self.template_vars
            .set("Date", &now.format("%Y-%m-%d").to_string());
        self.template_vars
            .set("Timestamp", &now.timestamp().to_string());
        self.template_vars.set("Now", &now.to_rfc3339());
    }

    /// Populate runtime environment variables.
    ///
    /// Sets:
    /// - `RuntimeGoos` — host OS (e.g. "linux", "macos", "windows")
    /// - `RuntimeGoarch` — host architecture (e.g. "x86_64", "aarch64")
    pub fn populate_runtime_vars(&mut self) {
        self.template_vars.set("RuntimeGoos", std::env::consts::OS);
        self.template_vars
            .set("RuntimeGoarch", std::env::consts::ARCH);
    }

    /// Populate the `ReleaseNotes` template variable from stored changelogs.
    ///
    /// Should be called after the changelog stage has run and populated
    /// `self.changelogs`. Uses the first crate (by config order) whose
    /// changelog is present, or an empty string if no changelogs exist.
    /// Config order is deterministic, unlike HashMap iteration order.
    pub fn populate_release_notes_var(&mut self) {
        // Look up changelogs in config-defined crate order for determinism.
        let notes = self
            .config
            .crates
            .iter()
            .find_map(|c| self.changelogs.get(&c.name))
            .cloned()
            .unwrap_or_default();
        self.template_vars.set("ReleaseNotes", &notes);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::{GitInfo, SemVer};

    fn make_git_info(dirty: bool, prerelease: Option<&str>) -> GitInfo {
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
            remote_url: "https://github.com/test/repo.git".to_string(),
            summary: "v1.2.3-0-gabc123d".to_string(),
            tag_subject: "Release v1.2.3".to_string(),
            tag_contents: "Release v1.2.3\n\nFull release notes here.".to_string(),
            tag_body: "Full release notes here.".to_string(),
        }
    }

    #[test]
    fn test_context_template_vars() {
        let mut config = Config::default();
        config.project_name = "test-project".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"test-project".to_string())
        );
    }

    #[test]
    fn test_context_should_skip() {
        let config = Config::default();
        let opts = ContextOptions {
            skip_stages: vec!["publish".to_string(), "announce".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(config, opts);
        assert!(ctx.should_skip("publish"));
        assert!(ctx.should_skip("announce"));
        assert!(!ctx.should_skip("build"));
    }

    #[test]
    fn test_context_render_template() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        let result = ctx.render_template("{{ .ProjectName }}-release").unwrap();
        assert_eq!(result, "myapp-release");
    }

    #[test]
    fn test_populate_git_vars_sets_all_expected_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("Major"), Some(&"1".to_string()));
        assert_eq!(v.get("Minor"), Some(&"2".to_string()));
        assert_eq!(v.get("Patch"), Some(&"3".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"".to_string()));
        assert_eq!(
            v.get("FullCommit"),
            Some(&"abc123def456abc123def456abc123def456abc1".to_string())
        );
        assert_eq!(v.get("ShortCommit"), Some(&"abc123d".to_string()));
        assert_eq!(v.get("Branch"), Some(&"main".to_string()));
        assert_eq!(
            v.get("CommitDate"),
            Some(&"2026-03-25T10:30:00+00:00".to_string())
        );
        assert_eq!(v.get("CommitTimestamp"), Some(&"1774463400".to_string()));
        assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
    }

    #[test]
    fn test_commit_is_alias_for_full_commit() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Commit"), v.get("FullCommit"));
    }

    #[test]
    fn test_populate_git_vars_prerelease() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, Some("rc.1")));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Version"), Some(&"1.2.3-rc.1".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"rc.1".to_string()));
    }

    #[test]
    fn test_git_tree_state_clean() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("IsGitDirty"), Some(&"false".to_string()));
        assert_eq!(v.get("GitTreeState"), Some(&"clean".to_string()));
    }

    #[test]
    fn test_git_tree_state_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("IsGitDirty"), Some(&"true".to_string()));
        assert_eq!(v.get("GitTreeState"), Some(&"dirty".to_string()));
    }

    #[test]
    fn test_is_snapshot_reflects_context_options() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );

        // Non-snapshot
        let config2 = Config::default();
        let opts2 = ContextOptions {
            snapshot: false,
            ..Default::default()
        };
        let mut ctx2 = Context::new(config2, opts2);
        ctx2.git_info = Some(make_git_info(false, None));
        ctx2.populate_git_vars();

        assert_eq!(
            ctx2.template_vars().get("IsSnapshot"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_draft_defaults_to_false() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_previous_tag_empty_when_none() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PreviousTag"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_time_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_time_vars();

        let v = ctx.template_vars();

        // Date should be YYYY-MM-DD format
        let date = v.get("Date").expect("Date should be set");
        assert!(
            date.len() == 10 && date.chars().nth(4) == Some('-'),
            "Date should be YYYY-MM-DD, got: {date}"
        );

        // Timestamp should be numeric
        let ts = v.get("Timestamp").expect("Timestamp should be set");
        assert!(
            ts.parse::<i64>().is_ok(),
            "Timestamp should be a numeric string, got: {ts}"
        );

        // Now should be ISO 8601
        let now = v.get("Now").expect("Now should be set");
        assert!(now.contains('T'), "Now should be ISO 8601, got: {now}");
    }

    #[test]
    fn test_env_vars_accessible_in_templates() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set_env("MY_VAR", "hello-world");
        ctx.template_vars_mut().set_env("DEPLOY_ENV", "staging");

        let result = ctx
            .render_template("{{ .Env.MY_VAR }}-{{ .Env.DEPLOY_ENV }}")
            .unwrap();
        assert_eq!(result, "hello-world-staging");
    }

    #[test]
    fn test_populate_git_vars_without_git_info_still_sets_snapshot() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // Don't set git_info — populate_git_vars should still set IsSnapshot/IsDraft
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
        );
        // Git-specific vars should NOT be set
        assert_eq!(ctx.template_vars().get("Tag"), None);
    }

    #[test]
    fn test_is_nightly_set_when_nightly_mode_active() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
            "IsNightly should be 'true' when nightly mode is active"
        );
        assert!(ctx.is_nightly(), "is_nightly() should return true");
    }

    #[test]
    fn test_is_nightly_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"false".to_string()),
            "IsNightly should default to 'false'"
        );
        assert!(
            !ctx.is_nightly(),
            "is_nightly() should return false by default"
        );
    }

    #[test]
    fn test_version_returns_populated_value() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(ctx.version(), "1.2.3");
    }

    #[test]
    fn test_version_returns_empty_when_not_set() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(ctx.version(), "");
    }

    #[test]
    fn test_is_nightly_without_git_info() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // No git_info set — populate_git_vars still sets IsNightly
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
            "IsNightly should be set even without git info"
        );
    }

    #[test]
    fn test_is_git_clean_when_not_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsGitClean"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_git_clean_when_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsGitClean"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_git_url_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("GitURL"),
            Some(&"https://github.com/test/repo.git".to_string())
        );
    }

    #[test]
    fn test_summary_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("Summary"),
            Some(&"v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_tag_subject_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagSubject"),
            Some(&"Release v1.2.3".to_string())
        );
    }

    #[test]
    fn test_tag_contents_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagContents"),
            Some(&"Release v1.2.3\n\nFull release notes here.".to_string())
        );
    }

    #[test]
    fn test_tag_body_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagBody"),
            Some(&"Full release notes here.".to_string())
        );
    }

    #[test]
    fn test_is_single_target_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_single_target_true_when_set() {
        let config = Config::default();
        let opts = ContextOptions {
            single_target: Some("x86_64-unknown-linux-gnu".to_string()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_populate_runtime_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_runtime_vars();

        let v = ctx.template_vars();

        let goos = v.get("RuntimeGoos").expect("RuntimeGoos should be set");
        assert!(
            !goos.is_empty(),
            "RuntimeGoos should not be empty, got: {goos}"
        );
        assert_eq!(goos, std::env::consts::OS);

        let goarch = v.get("RuntimeGoarch").expect("RuntimeGoarch should be set");
        assert!(
            !goarch.is_empty(),
            "RuntimeGoarch should not be empty, got: {goarch}"
        );
        assert_eq!(goarch, std::env::consts::ARCH);
    }

    #[test]
    fn test_populate_release_notes_var_with_changelogs() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "my-crate".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.changelogs
            .insert("my-crate".to_string(), "## Changes\n- fix bug".to_string());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"## Changes\n- fix bug".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_empty_when_no_changelogs() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_deterministic_with_multiple_crates() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "crate-a".to_string(),
            ..Default::default()
        });
        config.crates.push(crate::config::CrateConfig {
            name: "crate-b".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.changelogs
            .insert("crate-a".to_string(), "notes-a".to_string());
        ctx.changelogs
            .insert("crate-b".to_string(), "notes-b".to_string());
        ctx.populate_release_notes_var();

        // Should always pick the first crate in config order, not arbitrary HashMap order
        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"notes-a".to_string())
        );
    }
}
