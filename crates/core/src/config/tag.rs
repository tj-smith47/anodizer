use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::HookEntry;

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TagConfig {
    /// Default version bump type when no conventional commit token is found: "major", "minor", "patch", or "none".
    pub default_bump: Option<String>,
    /// Prefix prepended to version tags (e.g., "v" produces "v1.2.3").
    pub tag_prefix: Option<String>,
    /// Branch name patterns (supports wildcards) that trigger releases (default: ["master", "main"]).
    pub release_branches: Option<Vec<String>>,
    /// Custom version tag to use instead of auto-incrementing.
    pub custom_tag: Option<String>,
    /// Source for determining the previous tag: "repo" (default) or "branch".
    pub tag_context: Option<String>,
    /// Branch history mode for determining the previous tag: "full" or "last".
    pub branch_history: Option<String>,
    /// Version string to use when no previous tag exists (default: "0.1.0").
    pub initial_version: Option<String>,
    /// When true, apply a pre-release suffix to the generated version.
    pub prerelease: Option<bool>,
    /// Suffix appended to pre-release versions (e.g., "beta").
    pub prerelease_suffix: Option<String>,
    /// When true, create a new tag even if no commits have changed since the last tag.
    pub force_without_changes: Option<bool>,
    /// Like force_without_changes but only for pre-release versions.
    pub force_without_changes_pre: Option<bool>,
    /// Conventional commit token triggering a major bump (default: "major").
    pub major_string_token: Option<String>,
    /// Conventional commit token triggering a minor bump (default: "minor" or "feat").
    pub minor_string_token: Option<String>,
    /// Conventional commit token triggering a patch bump (default: "patch" or "fix").
    pub patch_string_token: Option<String>,
    /// Conventional commit token suppressing a version bump entirely (default: "none").
    pub none_string_token: Option<String>,
    /// When true, use the GitHub/GitLab API for tagging instead of git CLI.
    pub git_api_tagging: Option<bool>,
    /// When true, `anodizer tag` also pushes the version-sync bump commit to the
    /// release branch (atomically with the tag), not just the tag. CLI `--push` /
    /// `--no-push` override this. Default false preserves the "push the tag,
    /// inspect the branch locally before pushing" workflow.
    pub push: Option<bool>,
    /// Append `[skip ci]` to the version-sync bump commit subject.
    ///
    /// Off by default. Only enable with a `workflow_run`-triggered release
    /// workflow: `[skip ci]` on the bump commit (which becomes the tag target)
    /// ALSO suppresses an `on: push: tags:` release trigger, so enabling this
    /// with a tag-push-triggered release silently skips the release. Leave off
    /// for the GoReleaser-style tag-push pattern; enable for the `workflow_run`
    /// pattern to skip the (already crate-gated, harmless) redundant CI re-run.
    pub skip_ci_on_bump: Option<bool>,
    /// When true, print verbose tag calculation output.
    pub verbose: Option<bool>,
    /// Commands to run before `anodizer tag` creates the tag. Useful for updating
    /// lockfiles or committing sibling changes that must be part of the tagged
    /// commit. Env: `ANODIZER_CURRENT_TAG`, `ANODIZER_PREVIOUS_TAG` are set;
    /// template vars `{{ .Tag }}`, `{{ .PreviousTag }}`, `{{ .Version }}`,
    /// `{{ .PrefixedTag }}` are available.
    pub tag_pre_hooks: Option<Vec<HookEntry>>,
    /// Commands to run after `anodizer tag` successfully creates and pushes the
    /// tag. Env and template vars same as `tag_pre_hooks`.
    pub tag_post_hooks: Option<Vec<HookEntry>>,
}
