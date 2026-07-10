//! Shared helpers for the per-publisher modules in `stage-publish`.
//!
//! Each submodule groups one concern:
//!
//! - [`config`] — config / context lookups & resolution helpers.
//! - [`formats`] — package-format defaults + filename matching.
//! - [`cmd`] — the `run_cmd_in` subprocess helper.
//! - [`clone`] — repo cloning (HTTPS-token + SSH variants + dispatcher).
//! - [`commit`] — `CommitOptions` + `commit_and_push_with_opts`.
//! - [`branch`] — branch resolution + GitHub default-branch lookup.
//! - [`pr`] — PR submission flows (gh CLI + REST API).
//! - [`attribution`] — published-content attribution strings (PR footers,
//!   generated-file headers).
//! - [`artifacts`] — OS/arch inference + `OsArtifact` + filtering helpers.
//! - [`template`] — `render_url_template` and `render_or_warn`.
//! - [`parallelism`] — shared `ROLLBACK_PARALLELISM` cap for publishers
//!   that fan out per-target rollback work.
//!
//! Per-publisher modules reach these helpers through the `crate::util::IDENT`
//! paths re-exported below.

mod artifacts;
mod attribution;
mod branch;
mod clone;
mod cmd;
mod commit;
mod config;
mod disambiguate;
mod formats;
mod git_revert;
mod github_pr;
mod parallelism;
mod pr;
mod template;

#[cfg(test)]
mod tests;

// External re-export: every caller in this crate that previously wrote
// `crate::util::matches_id_filter` continues to work.
pub(crate) use anodizer_core::artifact::matches_id_filter;
pub(crate) use attribution::{GENERATED_FILE_HEADER, SUBMITTED_BY_FOOTER};

// Public surface preserved for external callers. Items with no current
// out-of-`util/` caller are intentionally NOT re-exported here — they
// remain `pub(crate)` inside their submodule and are reachable via
// `crate::util::<submod>::IDENT` if a future caller needs them. Adding
// a re-export trips `unused_imports` warnings under `-D warnings` so
// only living surface is exported.
pub(crate) use artifacts::{
    OsArtifact, filter_by_ids, find_all_platform_artifacts_with_variant,
    find_artifacts_by_os_with_variant,
};
pub(crate) use branch::resolve_branch;
pub(crate) use clone::{
    aur_default_git_url, clone_repo, clone_repo_ssh, clone_repo_with_auth, ssh_auth_probe,
};
pub(crate) use cmd::{run_cmd_in, run_cmd_in_timeout};
pub(crate) use commit::{
    CommitOutcome, GIT_FETCH_TIMEOUT, commit_and_push_with_opts, resolve_commit_opts,
};
pub(crate) use config::{
    find_crate_in_universe, get_publish_config, resolve_artifact_kind, resolve_repo_owner_name,
    resolve_repo_token, resolve_rollback_token, resolve_secret_name, should_skip_publisher_with_if,
    should_skip_upload,
};
pub(crate) use disambiguate::{DisambiguateConfig, disambiguate_by_format};
#[cfg(test)]
pub(crate) use disambiguate::{
    InnerConfig as DisambiguateInnerConfig, disambiguate_by_format_with_sink,
};
pub(crate) use formats::{default_package_formats, format_matches};
pub(crate) use git_revert::RevertTarget;
// `FindPrError` is reached through its `Display` impl only — krew's
// rollback formats `{e}` directly without naming variants. Keeping it
// out of the re-export surface honours the "only living surface" rule
// in the module rustdoc above.
pub(crate) use github_pr::{
    CloseOutcome, close_pr_via_api, close_pr_via_api_with_env, find_open_pr_numbers_for_head,
    find_open_pr_numbers_for_head_with_env,
};
pub(crate) use parallelism::{
    ROLLBACK_PARALLELISM, join_or_warn, lock_recover, run_revert_targets_parallel,
    run_token_revert_rollback,
};
pub(crate) use pr::{
    PrOrigin, SubmitPrOpts, maybe_submit_pr, maybe_submit_pr_with_env, submit_pr_via_gh_with_opts,
    submit_pr_via_gh_with_opts_with_env,
};
pub(crate) use template::{
    guard_no_unrendered, render_or_warn, render_or_warn_with_vars, render_url_template,
    render_url_template_with_ctx, render_url_template_with_ctx_and_artifact,
};
