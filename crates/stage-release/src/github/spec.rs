//! Backend input/output types and pure decision helpers for the GitHub
//! release run.
//!
//! Hosts the argument-cluster structs ([`BackendEnv`], [`GithubReleaseSpec`],
//! [`UploadOpts`]) consumed by [`super::backend::run_github_backend`] plus the
//! I/O-free classifiers ([`classify_already_exists`],
//! [`check_existing_assets_block_upload`], [`nightly_releases_to_prune`],
//! [`upload_retry_locals`]) so the branching logic is unit-testable without a
//! live octocrab client.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use octocrab::repos::releases::MakeLatest;

/// Runtime / context infrastructure for [`run_github_backend`].
///
/// Bundles the four "ambient" handles every backend call needs: the
/// shared tokio runtime, the global anodizer [`Context`], the per-stage
/// logger, and the resolved GitHub token. Pulling them into a struct
/// drains four positional arguments off the call site.
pub(crate) struct BackendEnv<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub ctx: &'a Context,
    pub log: &'a StageLogger,
    pub token: &'a Option<String>,
}

/// Per-release attributes consumed by [`run_github_backend`].
///
/// Mirrors `GitlabReleaseSpec` / `GiteaReleaseSpec` from the sibling
/// `gitlab.rs` / `gitea.rs` backends. Field names line up with
/// [`crate::release_body::ReleaseJsonSpec`] so the `build_release_json`
/// call site is a near-direct field forward.
#[derive(Clone, Copy)]
pub(crate) struct GithubReleaseSpec<'a> {
    pub tag: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub mode: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub make_latest: &'a Option<MakeLatest>,
    pub target_commitish: &'a Option<String>,
    pub discussion_category: &'a Option<String>,
}

/// Cluster controlling upload + retention semantics for [`run_github_backend`].
#[derive(Clone)]
pub(crate) struct UploadOpts {
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub replace_existing_artifacts: bool,
    pub use_existing_draft: bool,
    /// `--resume-release`: bypass the leftover-assets pre-check so the
    /// upload loop runs against an existing release left by a prior failed
    /// attempt.
    pub resume_release: bool,
    /// Nightly retention: keep the N newest nightly releases (matched by the
    /// rendered nightly name) and delete the rest before creating the new
    /// one, including the git tags anodizer created for them. `keep_last: 1`
    /// is the rolling-single-release case (`keep_single_release`); `None`
    /// disables the sweep. Operates on [`Self::publish_repo_override`] when
    /// set. Resolution of the legacy `keep_single_release` alias vs the
    /// `retention:` block happens upstream in
    /// [`anodizer_core::config::NightlyConfig::resolved_keep_last`], so this
    /// field is the single source of truth for the backend.
    pub retention_keep_last: Option<usize>,
    /// Nightly `publish_repo`: redirect the release create, asset upload, AND
    /// retention delete calls to a DIFFERENT `(owner, repo)` than the source
    /// repo resolved from `release.github`. `None` = source repo, unchanged.
    pub publish_repo_override: Option<(String, String)>,
}

/// Outcome for the upload-asset 422 `already_exists` decision branch.
/// Extracted from the body of [`run_github_backend`] so the logic can be
/// unit-tested without standing up a fake octocrab.
///
/// 422 upload-conflict decision rule:
///
/// ```text
/// if resp.StatusCode == http.StatusUnprocessableEntity {
///     if !ctx.Config.Release.ReplaceExistingArtifacts {
///         return retryx.Unrecoverable(err)
///     }
///     // delete + retry
/// }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AlreadyExistsAction {
    /// Local + remote bytes match: treat as a no-op (idempotency); a
    /// prior attempt in this same release already uploaded the file.
    SkipIdempotent,
    /// `replace_existing_artifacts: false` and bytes differ: bail with
    /// the conflict instead of overwriting.
    BailReplaceForbidden,
    /// Different bytes and the user opted in via
    /// `replace_existing_artifacts: true`: delete the stale asset and
    /// retry the upload.
    DeleteAndRetry,
}

/// Check whether an existing release's assets block a retry when
/// `replace_existing_artifacts` is false. Returns the list of asset names
/// that would conflict, or `None` when uploads may proceed.
///
/// Pure function so the pre-check logic can be unit-tested without I/O.
/// Returns `None` (uploads proceed) when ANY of:
///   - `skip_upload` is true (nothing will be uploaded),
///   - `resume_release` is true (the user explicitly opted into continuing
///     into a leftover release via `--resume-release`),
///   - `replace_existing_artifacts` is true (overwrites are permitted), or
///   - no assets exist on the release yet.
pub(crate) fn check_existing_assets_block_upload(
    skip_upload: bool,
    resume_release: bool,
    replace_existing_artifacts: bool,
    existing_asset_names: &[&str],
) -> Option<Vec<String>> {
    if skip_upload
        || resume_release
        || replace_existing_artifacts
        || existing_asset_names.is_empty()
    {
        return None;
    }
    Some(existing_asset_names.iter().map(|s| s.to_string()).collect())
}

/// Decide what to do when the GitHub upload-asset API returns
/// `422 already_exists`. Pure function so the (re-)introduced
/// `replace_existing_artifacts: false` guard can be tested without I/O.
pub(crate) fn classify_already_exists(
    replace_existing_artifacts: bool,
    remote_size: Option<u64>,
    local_size: u64,
) -> AlreadyExistsAction {
    // Idempotency check first: bytes that already match the local
    // artifact aren't an "overwrite", so the user's
    // `replace_existing_artifacts: false` does NOT block this path.
    if remote_size == Some(local_size) {
        return AlreadyExistsAction::SkipIdempotent;
    }
    if !replace_existing_artifacts {
        return AlreadyExistsAction::BailReplaceForbidden;
    }
    AlreadyExistsAction::DeleteAndRetry
}

/// Decide which nightly releases to prune so that — after the about-to-be-created
/// release is added — exactly `keep_last` nightly releases survive.
///
/// `releases` is the set of existing releases (`(id, tag)`) whose `name` matches
/// the nightly release name. They are sorted newest-first internally by release
/// `id` descending — monotonic with creation order on a single repo — so
/// correctness does not depend on the order GitHub returns them. Because the new
/// release will become the newest of the kept set, the prune target is "every
/// release beyond the newest `keep_last - 1`": that leaves `keep_last - 1` old
/// releases plus the new one = `keep_last`.
///
/// For `keep_last = 1` this returns ALL existing nightly releases — the rolling
/// single-release semantics (only the just-created release survives). This is the
/// single function both the `keep_single_release` alias and `retention.keep_last`
/// route through; there is no parallel single-delete path.
///
/// Pure (no I/O) so the keep/delete arithmetic is unit-testable without octocrab.
pub(crate) fn nightly_releases_to_prune(
    releases: &[(u64, String)],
    keep_last: usize,
) -> Vec<(u64, String)> {
    let keep_last = keep_last.max(1);
    // Sort newest-first by id descending so the keep/prune split is correct
    // regardless of the API response order.
    let mut sorted = releases.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.0));
    // The new release occupies one of the kept slots, so retain `keep_last - 1`
    // of the existing (newest-first) set and prune the remainder.
    sorted.into_iter().skip(keep_last - 1).collect()
}

/// Resolve the upload retry loop's per-iteration locals from a [`RetryPolicy`].
///
/// Returns `(max_upload_attempts, initial_retry_delay, max_retry_delay)` in
/// the order the upload loop binds them. The single point of translation
/// from policy to locals lives here so a future formula change is visible
/// in one place (and so tests can pin the formula against the backend without
/// re-deriving it inline).
///
/// `max_upload_attempts` mirrors [`RetryPolicy::max_attempts`] directly:
/// the `>= 1` invariant is enforced by [`anodizer_core::config::RetryConfig::to_policy`]
/// (clamps `attempts: 0` -> `1`) and `retry_async` / `retry_sync` (defensive
/// clamp at the loop boundary). No additional clamp is needed at the call
/// site.
pub(crate) fn upload_retry_locals(
    policy: &anodizer_core::retry::RetryPolicy,
) -> (u32, std::time::Duration, std::time::Duration) {
    (policy.max_attempts, policy.base_delay, policy.max_delay)
}

#[cfg(test)]
mod already_exists_tests {
    use super::*;

    #[test]
    fn idempotent_when_remote_matches_local_regardless_of_flag() {
        // Even with `replace_existing_artifacts: false`, a byte-identical
        // remote asset is a no-op: the user's guard rail is "don't
        // overwrite different bytes", not "don't probe the API".
        assert_eq!(
            classify_already_exists(false, Some(100), 100),
            AlreadyExistsAction::SkipIdempotent,
        );
        assert_eq!(
            classify_already_exists(true, Some(100), 100),
            AlreadyExistsAction::SkipIdempotent,
        );
    }

    #[test]
    fn bails_when_replace_forbidden_and_sizes_differ() {
        // `if !replace_existing_artifacts { return unrecoverable }`.
        // Surfaces the conflict instead of silently overwriting.
        assert_eq!(
            classify_already_exists(false, Some(100), 200),
            AlreadyExistsAction::BailReplaceForbidden,
        );
        // `remote_size: None` (asset present but size unknown) is treated
        // as a size-mismatch: better to bail than silently overwrite.
        assert_eq!(
            classify_already_exists(false, None, 200),
            AlreadyExistsAction::BailReplaceForbidden,
        );
    }

    #[test]
    fn deletes_and_retries_when_replace_allowed_and_sizes_differ() {
        assert_eq!(
            classify_already_exists(true, Some(100), 200),
            AlreadyExistsAction::DeleteAndRetry,
        );
        assert_eq!(
            classify_already_exists(true, None, 200),
            AlreadyExistsAction::DeleteAndRetry,
        );
    }
}

#[cfg(test)]
mod existing_assets_precheck_tests {
    use super::*;

    // Argument order across the helper:
    //   (skip_upload, resume_release, replace_existing_artifacts, asset_names)

    #[test]
    fn no_conflict_when_release_has_no_assets() {
        let result = check_existing_assets_block_upload(false, false, false, &[]);
        assert!(result.is_none(), "empty asset list must not block");
    }

    #[test]
    fn no_conflict_when_replace_existing_is_true() {
        let result = check_existing_assets_block_upload(false, false, true, &["foo.tar.gz"]);
        assert!(
            result.is_none(),
            "replace_existing_artifacts=true permits overwrite"
        );
    }

    #[test]
    fn no_conflict_when_skip_upload_is_true() {
        let result = check_existing_assets_block_upload(true, false, false, &["foo.tar.gz"]);
        assert!(result.is_none(), "skip_upload=true means nothing to upload");
    }

    #[test]
    fn no_conflict_when_resume_release_is_true() {
        // `--resume-release` is the user's explicit opt-in to continue into
        // an existing release: the pre-check must NOT bail even when assets
        // are present and replace_existing_artifacts is false.
        let result =
            check_existing_assets_block_upload(false, true, false, &["foo.tar.gz", "bar.zip"]);
        assert!(
            result.is_none(),
            "--resume-release must bypass the pre-check"
        );
    }

    #[test]
    fn no_conflict_when_replace_existing_cli_override_is_true() {
        // The CLI override is plumbed via `replace_existing_artifacts: true`
        // in the helper signature (the caller ORs the config value with
        // ctx.options.replace_existing_artifacts before calling).
        // This pins that the helper treats the CLI-derived value the same
        // as the config-derived value.
        let result =
            check_existing_assets_block_upload(false, false, true, &["foo.tar.gz", "bar.zip"]);
        assert!(
            result.is_none(),
            "--replace-existing must bypass the pre-check via replace_existing_artifacts=true"
        );
    }

    #[test]
    fn conflicts_when_assets_present_and_replace_forbidden() {
        // The scenario that was previously unrecoverable: partial assets
        // from a prior failed attempt exist, and replace_existing_artifacts
        // is false. The helper must surface them so the caller can bail.
        let assets = &["app_linux_amd64.tar.gz", "checksums.txt"];
        let result = check_existing_assets_block_upload(false, false, false, assets);
        let names = result.expect("should detect conflict");
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"app_linux_amd64.tar.gz".to_string()));
        assert!(names.contains(&"checksums.txt".to_string()));
    }

    #[test]
    fn conflict_list_preserves_input_order() {
        // The helper returns the names in the order the caller supplied
        // them, so the resulting bail message lists assets in a predictable
        // (release-API) order. A future sort/dedupe regression would be
        // user-visible noise; pin the contract.
        let assets = &["a.tar.gz", "b.zip", "c.sig"];
        let names = check_existing_assets_block_upload(false, false, false, assets)
            .expect("conflict present");
        assert_eq!(
            names,
            vec![
                "a.tar.gz".to_string(),
                "b.zip".to_string(),
                "c.sig".to_string()
            ]
        );
    }

    #[test]
    fn skip_upload_wins_even_with_assets_and_no_replace() {
        // skip_upload short-circuits BEFORE the asset-list inspection runs.
        // Pinning this so a future refactor doesn't reorder the early-return
        // and accidentally surface a conflict during a no-op upload pass.
        let result = check_existing_assets_block_upload(true, false, false, &["x.tar.gz"]);
        assert!(
            result.is_none(),
            "skip_upload short-circuits unconditionally"
        );
    }
}

#[cfg(test)]
mod upload_retry_locals_tests {
    //! Pin the policy-to-locals translation that the bespoke upload retry
    //! loop reads on every iteration. The formula is trivial today but the
    //! rustdoc claims "single point of translation"; if a future change
    //! adds a clamp / fudge factor / multiplier here, these tests force
    //! that change to be conscious (and visible in one place).
    use super::*;
    use anodizer_core::retry::RetryPolicy;
    use std::time::Duration;

    #[test]
    fn returns_policy_fields_verbatim() {
        let policy = RetryPolicy {
            max_attempts: 7,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(30),
        };
        let (attempts, base, max) = upload_retry_locals(&policy);
        assert_eq!(
            attempts, 7,
            "max_attempts mirrors RetryPolicy::max_attempts"
        );
        assert_eq!(base, Duration::from_millis(50));
        assert_eq!(max, Duration::from_secs(30));
    }

    #[test]
    fn surfaces_the_upload_canonical_policy_unchanged() {
        // Canonical upload policy: 10 attempts, 50ms base,
        // 30s cap. The locals helper must NOT mutate these on the way to the
        // upload loop — drift here is a user-visible behaviour change in the
        // retry envelope.
        let (attempts, base, max) = upload_retry_locals(&RetryPolicy::UPLOAD);
        assert_eq!(attempts, 10);
        assert_eq!(base, Duration::from_millis(50));
        assert_eq!(max, Duration::from_secs(30));
    }

    #[test]
    fn preserves_one_attempt_minimum_without_extra_clamp() {
        // The rustdoc claims the helper relies on RetryConfig::to_policy's
        // upstream clamp and adds none of its own. A `max_attempts: 1`
        // input must therefore round-trip unchanged (proving the helper
        // does not, say, force a minimum of 2 retries).
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let (attempts, _, _) = upload_retry_locals(&policy);
        assert_eq!(
            attempts, 1,
            "single-attempt policy must round-trip verbatim"
        );
    }
}

#[cfg(test)]
mod already_exists_action_derive_tests {
    //! Pin the `Debug`/`PartialEq`/`Eq` derives on `AlreadyExistsAction`.
    //! The classifier returns these variants and downstream call sites in
    //! the upload retry loop `match` on them — a drift to a non-equality
    //! representation would silently break the upload loop's arm matching.
    use super::*;

    #[test]
    fn variants_compare_equal_only_to_themselves() {
        assert_eq!(
            AlreadyExistsAction::SkipIdempotent,
            AlreadyExistsAction::SkipIdempotent
        );
        assert_ne!(
            AlreadyExistsAction::SkipIdempotent,
            AlreadyExistsAction::BailReplaceForbidden
        );
        assert_ne!(
            AlreadyExistsAction::BailReplaceForbidden,
            AlreadyExistsAction::DeleteAndRetry
        );
        assert_ne!(
            AlreadyExistsAction::DeleteAndRetry,
            AlreadyExistsAction::SkipIdempotent
        );
    }

    #[test]
    fn debug_format_names_the_variant() {
        // The error-path log lines format the action via `{:?}` to identify
        // which branch the classifier picked. Pin the variant names so a
        // future rename (`SkipIdempotent` -> `Idempotent`) surfaces in the
        // log diff instead of silently breaking grep-based triage.
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::SkipIdempotent),
            "SkipIdempotent"
        );
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::BailReplaceForbidden),
            "BailReplaceForbidden"
        );
        assert_eq!(
            format!("{:?}", AlreadyExistsAction::DeleteAndRetry),
            "DeleteAndRetry"
        );
    }
}

#[cfg(test)]
mod spec_struct_surface_tests {
    //! Pin the field surface of the three "context bundles" passed
    //! into `run_github_backend`. Each is `Clone + Copy` so a struct
    //! can be constructed, copied, and read field-by-field through
    //! the copy — a future field removal/rename breaks compilation
    //! here, not at the distant call site in `run.rs`.
    use super::*;
    use octocrab::repos::releases::MakeLatest;

    #[test]
    fn github_release_spec_round_trips_all_fields() {
        let make_latest = Some(MakeLatest::True);
        let target = Some("main".to_string());
        let category = Some("Announcements".to_string());
        let spec = GithubReleaseSpec {
            tag: "v1.2.3",
            name: "Release 1.2.3",
            body: "## Changes",
            mode: "replace",
            draft: true,
            prerelease: false,
            make_latest: &make_latest,
            target_commitish: &target,
            discussion_category: &category,
        };
        let copy = spec; // exercises Copy
        assert_eq!(copy.tag, "v1.2.3");
        assert_eq!(copy.name, "Release 1.2.3");
        assert_eq!(copy.body, "## Changes");
        assert_eq!(copy.mode, "replace");
        assert!(copy.draft);
        assert!(!copy.prerelease);
        assert!(copy.make_latest.is_some());
        assert_eq!(copy.target_commitish.as_deref(), Some("main"));
        assert_eq!(copy.discussion_category.as_deref(), Some("Announcements"));
    }

    #[test]
    fn upload_opts_round_trips_every_field() {
        // Independent fields -> a drift in field order or a silent removal
        // would let the caller in `run.rs` send `replace_existing_draft`
        // where `skip_upload` was wanted. Pin each one by name.
        let opts = UploadOpts {
            skip_upload: true,
            replace_existing_draft: false,
            replace_existing_artifacts: true,
            use_existing_draft: false,
            resume_release: true,
            retention_keep_last: Some(10),
            publish_repo_override: Some(("nushell".to_string(), "nightly".to_string())),
        };
        let copy = opts.clone();
        assert!(copy.skip_upload);
        assert!(!copy.replace_existing_draft);
        assert!(copy.replace_existing_artifacts);
        assert!(!copy.use_existing_draft);
        assert!(copy.resume_release);
        assert_eq!(copy.retention_keep_last, Some(10));
        assert_eq!(
            copy.publish_repo_override,
            Some(("nushell".to_string(), "nightly".to_string()))
        );
    }

    #[test]
    fn upload_opts_all_false_is_constructible() {
        // The "default-ish" shape (no opt-ins): the upload loop must see
        // every flag as `false` so the production code path runs as the
        // Canonical default. A drift to e.g. `Option<bool>` would break
        // this all-false literal.
        let opts = UploadOpts {
            skip_upload: false,
            replace_existing_draft: false,
            replace_existing_artifacts: false,
            use_existing_draft: false,
            resume_release: false,
            retention_keep_last: None,
            publish_repo_override: None,
        };
        assert!(!opts.skip_upload);
        assert!(!opts.replace_existing_draft);
        assert!(!opts.replace_existing_artifacts);
        assert!(!opts.use_existing_draft);
        assert!(!opts.resume_release);
        assert_eq!(opts.retention_keep_last, None);
        assert_eq!(opts.publish_repo_override, None);
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_one_prunes_all() {
        // keep_last=1 (the keep_single_release alias): every existing nightly
        // release is pruned — only the about-to-be-created one survives.
        let existing = vec![
            (3u64, "0.1.0-nightly.2".to_string()),
            (2u64, "0.1.0-nightly.1".to_string()),
            (1u64, "0.1.0-nightly.0".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&existing, 1);
        assert_eq!(pruned, existing);
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_n_keeps_newest() {
        // keep_last=2: with the new release counting as the newest, retain
        // only the single newest existing release; prune the older two.
        let existing = vec![
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
            (1u64, "t1".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&existing, 2);
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())]
        );
    }

    #[test]
    fn nightly_releases_to_prune_keeps_all_when_under_budget() {
        // Fewer existing releases than (keep_last - 1): nothing to prune.
        let existing = vec![(1u64, "t1".to_string())];
        assert!(nightly_releases_to_prune(&existing, 10).is_empty());
    }

    #[test]
    fn nightly_releases_to_prune_floors_zero_to_one() {
        let existing = vec![(1u64, "t1".to_string())];
        // keep_last=0 floored to 1 -> prune everything.
        assert_eq!(nightly_releases_to_prune(&existing, 0), existing);
    }

    #[test]
    fn nightly_releases_to_prune_sorts_out_of_order_input() {
        // API response order must not matter: feed ids out of order and
        // assert the newest (highest id) is the one kept.
        let existing = vec![
            (1u64, "t1".to_string()),
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
        ];
        // keep_last=2: keep the single newest existing (id=3), prune 2 and 1
        // in newest-first order.
        let pruned = nightly_releases_to_prune(&existing, 2);
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())],
            "must keep the highest-id release regardless of input order",
        );
    }
}
