use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::HookEntry;

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TagConfig {
    /// Default bump when a commit range carries no explicit `#` token and no
    /// conventional-commit marker: "major", "minor", "patch", or "none".
    /// Defaults to "none" — a range of only chore/docs/style/refactor/test/
    /// build/ci commits produces no release (the conventional-commit contract).
    /// Set "patch"/"minor" to cut a release on every range regardless of type.
    pub default_bump: Option<String>,
    /// While the current major version is `0`, demote a conventional breaking
    /// change (`feat!:` / `BREAKING CHANGE`) from a major bump to a minor bump
    /// (e.g. `0.5.0` → `0.6.0` instead of `1.0.0`). Honors the SemVer rule that
    /// anything may change in the `0.y.z` range. An explicit `#major`/`#minor`
    /// token, a `custom_tag`, or a manually-ahead `Cargo.toml` version always
    /// wins over this demotion. No-op once a real tag reaches `1.x`. Default false.
    pub bump_minor_pre_major: Option<bool>,
    /// While the current major version is `0`, demote a conventional feature
    /// (`feat:`) from a minor bump to a patch bump (e.g. `0.5.0` → `0.5.1`
    /// instead of `0.6.0`). Independent of `bump_minor_pre_major`. An explicit
    /// token / `custom_tag` / ahead `Cargo.toml` always wins. No-op at `1.x`.
    /// Default false.
    pub bump_patch_for_minor_pre_major: Option<bool>,
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
    ///
    /// Mutually exclusive with `sign` on a pushed tag: the API mints the tag
    /// object server-side and cannot apply your local GPG/SSH signature, so
    /// anodizer errors rather than shipping a silently-unsigned tag. Use local
    /// tagging (drop `git_api_tagging`) to create signed tags.
    pub git_api_tagging: Option<bool>,
    /// When true, anodizer creates the version tag with `git tag -s` (a
    /// cryptographically signed annotated tag) instead of the default `git tag
    /// -a` (unsigned annotated tag). The signing key and method are taken
    /// entirely from the user's git configuration (`user.signingkey`, and
    /// `gpg.format` to select GPG vs SSH signing) — anodizer adds no key field
    /// of its own, so both GPG and SSH signing work with no anodizer-specific
    /// setup. Applies in every workspace mode: single-crate, lockstep, and
    /// per-crate — every tag anodizer cuts is signed when this is enabled. The
    /// CLI `--sign` / `--no-sign` flags override this per invocation. Default
    /// (unset or false) leaves the existing unsigned annotated-tag behavior
    /// unchanged.
    ///
    /// Mutually exclusive with `git_api_tagging` on a pushed tag: the GitHub API
    /// creates the tag object on the remote and cannot apply a local signature,
    /// so combining the two on a push is a hard error. Signing works with local
    /// tagging and with `--push-tags-only` (both cut the tag locally first).
    pub sign: Option<bool>,
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
    /// for the tag-push pattern; enable for the `workflow_run`
    /// pattern to skip the (already crate-gated, harmless) redundant CI re-run.
    pub skip_ci_on_bump: Option<bool>,
    /// When true, print verbose tag calculation output.
    pub verbose: Option<bool>,
    /// Commands to run before `anodizer tag` creates the tag. Useful for updating
    /// lockfiles or committing sibling changes that must be part of the tagged
    /// commit. Env: `ANODIZER_CURRENT_TAG`, `ANODIZER_PREVIOUS_TAG` are set;
    /// template vars `{{ Tag }}`, `{{ PreviousTag }}`, `{{ Version }}`,
    /// `{{ PrefixedTag }}` are available.
    pub tag_pre_hooks: Option<Vec<HookEntry>>,
    /// Commands to run after `anodizer tag` successfully creates the tag (and,
    /// when a push was requested, pushes it). Env and template vars same as
    /// `tag_pre_hooks`.
    pub tag_post_hooks: Option<Vec<HookEntry>>,
}
