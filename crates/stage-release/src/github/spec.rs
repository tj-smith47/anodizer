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

/// Runtime / context infrastructure for [`run_github_backend`](crate::github::backend::run_github_backend).
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

/// Per-release attributes consumed by [`run_github_backend`](crate::github::backend::run_github_backend).
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

/// Cluster controlling upload + retention semantics for [`run_github_backend`](crate::github::backend::run_github_backend).
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
    /// rendered nightly name) and delete the rest AFTER the new release is
    /// created and published, including the git tags anodizer created for them.
    /// `keep_last: 1` is the rolling-single-release case (`keep_single_release`);
    /// `None` disables the sweep. Operates on [`Self::publish_repo_override`]
    /// when set. Resolution of the legacy `keep_single_release` alias vs the
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
/// Extracted from the body of [`run_github_backend`](crate::github::backend::run_github_backend) so the logic can be
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
/// `422 already_exists`. Pure function so the
/// `replace_existing_artifacts: false` guard can be tested without I/O.
///
/// A partial asset (`remote.uploaded == false` — GitHub registered the
/// name but the upload never completed, e.g. a transient 401/5xx broke
/// the transfer mid-flight) is ALWAYS `DeleteAndRetry`, regardless of
/// `replace_existing_artifacts`: it is this release's own debris, not
/// published content, and it blocks every same-name retry with
/// `already_exists` until deleted. GoReleaser's upload path has the
/// same delete-before-retry recovery.
///
/// For a fully-uploaded asset, a `422 already_exists` means the asset
/// definitively exists. When GitHub's per-asset `digest` is available on
/// both sides, same-size-but-different-digest is NOT idempotent (GPG/cosign
/// signatures embed timestamps/nonces, so a same-size resign is common and
/// must not be mistaken for the original upload surviving unchanged) — it is
/// classified the same as a size mismatch (`DeleteAndRetry` when replace is
/// allowed, `BailReplaceForbidden` otherwise). Only when a digest is
/// unavailable on either side does this fall back to the shared
/// [`classify_asset_conflict`](crate::classify_asset_conflict) size-only
/// rule; an unreadable probe (`None`) is treated as a mismatch there, matching
/// the conservative size-compare rule. The byte-identical-skip invariant for
/// the no-digest case lives in that shared classifier, not here.
pub(crate) fn classify_already_exists(
    replace_existing_artifacts: bool,
    remote: Option<super::assets::RemoteAssetProbe>,
    local_size: u64,
    local_digest: Option<&str>,
) -> AlreadyExistsAction {
    if remote.as_ref().is_some_and(|p| !p.uploaded) {
        return AlreadyExistsAction::DeleteAndRetry;
    }
    let remote_size = remote.as_ref().map(|p| p.size);
    if remote_size == Some(local_size) {
        if let (Some(remote_digest), Some(local_digest)) = (
            remote.as_ref().and_then(|p| p.digest.as_deref()),
            local_digest,
        ) {
            return if digests_match(remote_digest, local_digest) {
                AlreadyExistsAction::SkipIdempotent
            } else if replace_existing_artifacts {
                AlreadyExistsAction::DeleteAndRetry
            } else {
                AlreadyExistsAction::BailReplaceForbidden
            };
        }
    }
    match crate::classify_asset_conflict(replace_existing_artifacts, true, remote_size, local_size)
    {
        crate::AssetConflict::IdenticalSkip => AlreadyExistsAction::SkipIdempotent,
        crate::AssetConflict::ReplaceDiffering => AlreadyExistsAction::DeleteAndRetry,
        // A `422 already_exists` guarantees the asset is present, so the shared
        // classifier never returns `NoConflict` here; both remaining variants
        // mean "differs, overwrite forbidden" -> bail rather than mutate
        // published bytes.
        crate::AssetConflict::ConflictForbidden | crate::AssetConflict::NoConflict => {
            AlreadyExistsAction::BailReplaceForbidden
        }
    }
}

/// Compare a GitHub asset digest (`"sha256:<hex>"`) against a locally
/// computed hex digest, case-insensitively and tolerant of the `sha256:`
/// prefix being absent (defensive; the API always includes it today).
fn digests_match(remote_digest: &str, local_hex: &str) -> bool {
    let remote_hex = remote_digest
        .strip_prefix("sha256:")
        .unwrap_or(remote_digest);
    remote_hex.eq_ignore_ascii_case(local_hex)
}

/// The release track a nightly retention sweep is confined to.
///
/// Every track of a multitrack workspace renders the SAME nightly release
/// name, so a sweep keyed on the name alone deletes its siblings' releases —
/// and the git tags behind them. The track's identity lives in its tag
/// instead: the family the crate's `tag_template` mints
/// ([`anodizer_core::git::tag_in_family`]), which anchors on the template's
/// literal prefix and so keeps `v…-nightly` apart from `operator-v…-nightly`
/// where a suffix rule would alias them.
pub(crate) struct NightlyRetentionFamily<'a> {
    /// The tag of the release this run just published.
    pub(crate) tag: &'a str,
    /// The publishing crate's tag-family template.
    pub(crate) tag_template: &'a str,
    /// Every OTHER crate's tag-family template, so a narrower sibling family
    /// (`vault-v` under a bare `v`) claims its own tags back.
    pub(crate) sibling_templates: &'a [String],
    /// `monorepo.tag_prefix`, when configured.
    pub(crate) monorepo_prefix: Option<&'a str>,
    /// Whether the workspace mints more than one tag family.
    pub(crate) multitrack: bool,
}

impl NightlyRetentionFamily<'_> {
    /// Whether the sweep narrows to the family at all.
    ///
    /// Only a multitrack workspace has siblings to protect, and narrowing
    /// where there are none would strand every release cut under an earlier
    /// tag scheme (a repo that used to pin `nightly.tag_name` and now mints
    /// version-derived tags) — the name alone is already unambiguous there.
    /// A literal `nightly.tag_name` likewise mints a tag outside every
    /// family, leaving the name as the only key there is.
    pub(crate) fn scopes(&self) -> bool {
        self.multitrack && self.contains(self.tag)
    }

    /// Whether `tag` belongs to this track, with narrower sibling families
    /// excluded so a bare `v` never swallows `vault-v1.0.0-nightly`.
    pub(crate) fn contains(&self, tag: &str) -> bool {
        anodizer_core::git::tag_in_family_excluding_siblings(
            tag,
            self.tag_template,
            self.monorepo_prefix,
            self.sibling_templates,
        )
    }

    /// Narrow a name-matched release set to this track.
    pub(crate) fn scope(&self, releases: &[(u64, String)]) -> Vec<(u64, String)> {
        if !self.scopes() {
            return releases.to_vec();
        }
        releases
            .iter()
            .filter(|(_, rel_tag)| self.contains(rel_tag))
            .cloned()
            .collect()
    }

    /// The family as a diagnostic, for the sweep's verbose line.
    ///
    /// Names the excluded siblings (`v* minus operator-v*, csi-v*`) because
    /// the own-glob alone reads as "everything starting with v" — the exact
    /// misreading that makes an operator think the sweep is about to delete a
    /// sibling track's releases.
    pub(crate) fn describe(&self) -> String {
        let Some(own) =
            anodizer_core::git::tag_family_glob(self.tag_template, self.monorepo_prefix)
        else {
            return "(unscoped)".to_string();
        };
        let own_prefix =
            anodizer_core::git::tag_family_prefix(self.tag_template, self.monorepo_prefix)
                .unwrap_or_default();
        let mut excluded: Vec<String> = Vec::new();
        for sib in self.sibling_templates {
            let Some(prefix) = anodizer_core::git::tag_family_prefix(sib, self.monorepo_prefix)
            else {
                continue;
            };
            if prefix.len() <= own_prefix.len() {
                continue;
            }
            let Some(glob) = anodizer_core::git::tag_family_glob(sib, self.monorepo_prefix) else {
                continue;
            };
            if !excluded.contains(&glob) {
                excluded.push(glob);
            }
        }
        if excluded.is_empty() {
            own
        } else {
            format!("{own} minus {}", excluded.join(", "))
        }
    }
}

/// Decide which nightly releases to prune so that exactly `keep_last` nightly
/// releases survive — run AFTER the new release is created and published.
///
/// `releases` is the full set of releases (`(id, tag)`) whose `name` matches the
/// nightly release name, INCLUDING the just-created `protect_id`. `family`
/// narrows that set to the publishing crate's own release track, so a
/// multitrack workspace — where every track renders the same name — prunes
/// only its own history. They are sorted
/// newest-first internally by release `id` descending — monotonic with creation
/// order on a single repo — so correctness does not depend on the order GitHub
/// returns them. The newest `keep_last` survive; everything older is pruned.
///
/// `protect_id` is the id of the release just created/published this run. It is
/// NEVER returned for pruning even if it somehow sorts outside the newest
/// `keep_last` window: deleting the release that was just made live would defeat
/// the retention sweep's irreversible-before-reversible ordering. The new release
/// is the highest id (creation is monotonic), so it normally tops the kept set;
/// the filter is the safety net.
///
/// For `keep_last = 1` this returns every release except `protect_id` — the
/// rolling-single-release semantics (only the just-created release survives).
/// This is the single function both the `keep_single_release` alias and
/// `retention.keep_last` route through; there is no parallel single-delete path.
///
/// Pure (no I/O) so the keep/delete arithmetic is unit-testable without octocrab.
pub(crate) fn nightly_releases_to_prune(
    releases: &[(u64, String)],
    keep_last: usize,
    protect_id: u64,
    family: &NightlyRetentionFamily<'_>,
) -> Vec<(u64, String)> {
    let keep_last = keep_last.max(1);
    // Sort newest-first by id descending so the keep/prune split is correct
    // regardless of the API response order.
    let mut sorted = family.scope(releases);
    sorted.sort_by_key(|r| std::cmp::Reverse(r.0));
    // The just-created release is counted in the kept set, so the newest
    // `keep_last` survive and everything older is pruned. The just-created
    // release is filtered out of the prune set unconditionally so a surprising
    // id ordering can never delete the release this run just published.
    sorted
        .into_iter()
        .skip(keep_last)
        .filter(|(id, _)| *id != protect_id)
        .collect()
}

/// Resolve the upload retry loop's per-iteration locals from a [`RetryPolicy`](anodizer_core::retry::RetryPolicy).
///
/// Returns `(max_upload_attempts, initial_retry_delay, max_retry_delay)` in
/// the order the upload loop binds them. The single point of translation
/// from policy to locals lives here so a future formula change is visible
/// in one place (and so tests can pin the formula against the backend without
/// re-deriving it inline).
///
/// `max_upload_attempts` mirrors [`RetryPolicy::max_attempts`](anodizer_core::retry::RetryPolicy::max_attempts) directly:
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
    use super::super::assets::RemoteAssetProbe;
    use super::*;

    fn uploaded(size: u64) -> Option<RemoteAssetProbe> {
        Some(RemoteAssetProbe {
            size,
            uploaded: true,
            digest: None,
        })
    }

    fn uploaded_with_digest(size: u64, digest: &str) -> Option<RemoteAssetProbe> {
        Some(RemoteAssetProbe {
            size,
            uploaded: true,
            digest: Some(digest.to_string()),
        })
    }

    fn partial(size: u64) -> Option<RemoteAssetProbe> {
        Some(RemoteAssetProbe {
            size,
            uploaded: false,
            digest: None,
        })
    }

    #[test]
    fn idempotent_when_remote_matches_local_regardless_of_flag() {
        // Even with `replace_existing_artifacts: false`, a byte-identical
        // remote asset is a no-op: the user's guard rail is "don't
        // overwrite different bytes", not "don't probe the API".
        assert_eq!(
            classify_already_exists(false, uploaded(100), 100, None),
            AlreadyExistsAction::SkipIdempotent,
        );
        assert_eq!(
            classify_already_exists(true, uploaded(100), 100, None),
            AlreadyExistsAction::SkipIdempotent,
        );
    }

    #[test]
    fn bails_when_replace_forbidden_and_sizes_differ() {
        // `if !replace_existing_artifacts { return unrecoverable }`.
        // Surfaces the conflict instead of silently overwriting.
        assert_eq!(
            classify_already_exists(false, uploaded(100), 200, None),
            AlreadyExistsAction::BailReplaceForbidden,
        );
        // Probe `None` (422 says the asset exists but the list could not
        // see it) is treated as a size-mismatch: better to bail than
        // silently overwrite.
        assert_eq!(
            classify_already_exists(false, None, 200, None),
            AlreadyExistsAction::BailReplaceForbidden,
        );
    }

    #[test]
    fn deletes_and_retries_when_replace_allowed_and_sizes_differ() {
        assert_eq!(
            classify_already_exists(true, uploaded(100), 200, None),
            AlreadyExistsAction::DeleteAndRetry,
        );
        assert_eq!(
            classify_already_exists(true, None, 200, None),
            AlreadyExistsAction::DeleteAndRetry,
        );
    }

    #[test]
    fn partial_asset_deletes_and_retries_regardless_of_replace_flag() {
        // An interrupted upload leaves the asset in a non-"uploaded"
        // state. It is never published content, so it must be deleted
        // and re-uploaded even when overwrites are forbidden — and even
        // when its registered size happens to equal the local size.
        assert_eq!(
            classify_already_exists(false, partial(100), 200, None),
            AlreadyExistsAction::DeleteAndRetry,
        );
        assert_eq!(
            classify_already_exists(true, partial(100), 200, None),
            AlreadyExistsAction::DeleteAndRetry,
        );
        assert_eq!(
            classify_already_exists(false, partial(200), 200, None),
            AlreadyExistsAction::DeleteAndRetry,
            "size-equal partial must NOT be treated as idempotent",
        );
    }

    #[test]
    fn same_size_different_digest_is_not_idempotent() {
        // A GPG/cosign resign of byte-identical input yields a same-size,
        // different-digest asset (embedded timestamp/nonce). Size-only
        // comparison would wrongly call this idempotent; the digest check
        // must catch it and route through the normal conflict rule.
        assert_eq!(
            classify_already_exists(
                false,
                uploaded_with_digest(100, "sha256:aaaa"),
                100,
                Some("bbbb"),
            ),
            AlreadyExistsAction::BailReplaceForbidden,
        );
        assert_eq!(
            classify_already_exists(
                true,
                uploaded_with_digest(100, "sha256:aaaa"),
                100,
                Some("bbbb"),
            ),
            AlreadyExistsAction::DeleteAndRetry,
        );
    }

    #[test]
    fn same_size_same_digest_is_idempotent_case_insensitive_and_prefix_tolerant() {
        assert_eq!(
            classify_already_exists(
                false,
                uploaded_with_digest(100, "sha256:AAbb"),
                100,
                Some("aabb"),
            ),
            AlreadyExistsAction::SkipIdempotent,
        );
    }

    #[test]
    fn missing_remote_digest_falls_back_to_size_only_idempotency() {
        // GHES / older assets don't serve a digest — the classifier must
        // still treat a same-size asset as idempotent via the shared
        // size-only rule rather than bailing for lack of a digest.
        assert_eq!(
            classify_already_exists(false, uploaded(100), 100, Some("aabb")),
            AlreadyExistsAction::SkipIdempotent,
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

    /// A retention family that scopes NOTHING — a literal `nightly.tag_name`
    /// mints a tag outside every crate's family, so the release name stays the
    /// only key. The arithmetic pins below predate family scoping and must be
    /// unchanged by it.
    fn unscoped(tag: &str) -> NightlyRetentionFamily<'_> {
        NightlyRetentionFamily {
            tag,
            tag_template: "nightly",
            sibling_templates: &[],
            monorepo_prefix: None,
            multitrack: false,
        }
    }

    /// The family a per-crate `tag_template` mints, for the multitrack pins.
    fn family<'a>(tag: &'a str, tag_template: &'a str) -> NightlyRetentionFamily<'a> {
        NightlyRetentionFamily {
            tag,
            tag_template,
            sibling_templates: &[],
            monorepo_prefix: None,
            multitrack: true,
        }
    }

    /// A single-track workspace: one family, so nothing to narrow to.
    fn single_track<'a>(tag: &'a str, tag_template: &'a str) -> NightlyRetentionFamily<'a> {
        NightlyRetentionFamily {
            multitrack: false,
            ..family(tag, tag_template)
        }
    }

    /// A workspace whose crates leave `tag_template` UNSET still mints one
    /// family per crate (the `<name>-v` convention), so the sweep narrows —
    /// and a bare `v` sibling cannot claim `vault-v…`.
    #[test]
    fn nightly_retention_scopes_when_a_sibling_family_shares_a_prefix() {
        let siblings = vec!["vault-v{{ Version }}".to_string()];
        let family = NightlyRetentionFamily {
            tag: "v0.5.2-new-nightly",
            tag_template: "v{{ Version }}",
            sibling_templates: &siblings,
            monorepo_prefix: None,
            multitrack: true,
        };
        assert!(family.scopes());
        let all = vec![
            (1u64, "v0.5.0-old-nightly".to_string()),
            (2u64, "vault-v1.0.0-nightly".to_string()),
            (3u64, "v0.5.2-new-nightly".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&all, 1, 3, &family);
        assert_eq!(
            pruned,
            vec![(1u64, "v0.5.0-old-nightly".to_string())],
            "only this track's own older nightly is pruned; the `vault-v` \
             sibling's release must survive",
        );
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_one_prunes_all_but_new() {
        // keep_last=1 (the keep_single_release alias): the prune list (which
        // now includes the just-created release id=4) keeps only the new
        // release; every older nightly is pruned.
        let all = vec![
            (4u64, "v1.2.3".to_string()), // the just-created release
            (3u64, "0.1.0-nightly.2".to_string()),
            (2u64, "0.1.0-nightly.1".to_string()),
            (1u64, "0.1.0-nightly.0".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&all, 1, 4, &unscoped(&all[0].1));
        assert_eq!(
            pruned,
            vec![
                (3u64, "0.1.0-nightly.2".to_string()),
                (2u64, "0.1.0-nightly.1".to_string()),
                (1u64, "0.1.0-nightly.0".to_string()),
            ]
        );
    }

    #[test]
    fn nightly_releases_to_prune_never_prunes_the_new_release() {
        // The just-created release id MUST NOT appear in the prune list,
        // even at keep_last=1: deleting it would leave zero published nightly.
        let all = vec![
            (4u64, "v1.2.3".to_string()),
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
            (1u64, "t1".to_string()),
        ];
        for keep in [1usize, 2, 3, 4, 10] {
            let pruned = nightly_releases_to_prune(&all, keep, 4, &unscoped(&all[0].1));
            assert!(
                !pruned.iter().any(|(id, _)| *id == 4),
                "protect_id=4 must never be pruned (keep_last={keep}); got {pruned:?}",
            );
        }
    }

    #[test]
    fn nightly_releases_to_prune_protects_new_even_if_lowest_id() {
        // Defensive: if the just-created release somehow has the LOWEST id
        // (an out-of-order/API surprise), the protect filter still keeps it
        // out of the prune set rather than deleting the live release.
        let all = vec![
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
            (1u64, "new".to_string()), // protected, but lowest id
        ];
        let pruned = nightly_releases_to_prune(&all, 1, 1, &unscoped("new"));
        assert!(
            !pruned.iter().any(|(id, _)| *id == 1),
            "the protected (just-created) release must not be pruned: {pruned:?}",
        );
    }

    #[test]
    fn nightly_releases_to_prune_keep_last_n_keeps_newest() {
        // keep_last=2: with the new release (id=4) the newest, retain it plus
        // one older release; prune the rest.
        let all = vec![
            (4u64, "v1.2.3".to_string()),
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
            (1u64, "t1".to_string()),
        ];
        let pruned = nightly_releases_to_prune(&all, 2, 4, &unscoped(&all[0].1));
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())]
        );
    }

    #[test]
    fn nightly_releases_to_prune_keeps_all_when_under_budget() {
        // Fewer releases than keep_last: nothing to prune.
        let all = vec![(2u64, "v1.2.3".to_string()), (1u64, "t1".to_string())];
        assert!(nightly_releases_to_prune(&all, 10, 2, &unscoped("v1.2.3")).is_empty());
    }

    #[test]
    fn nightly_releases_to_prune_floors_zero_to_one() {
        let all = vec![(2u64, "v1.2.3".to_string()), (1u64, "t1".to_string())];
        // keep_last=0 floored to 1 -> prune everything except the new release.
        assert_eq!(
            nightly_releases_to_prune(&all, 0, 2, &unscoped("v1.2.3")),
            vec![(1u64, "t1".to_string())]
        );
    }

    #[test]
    fn nightly_releases_to_prune_sorts_out_of_order_input() {
        // API response order must not matter: feed ids out of order and
        // assert the newest (highest id) survives.
        let all = vec![
            (1u64, "t1".to_string()),
            (4u64, "v1.2.3".to_string()),
            (3u64, "t3".to_string()),
            (2u64, "t2".to_string()),
        ];
        // keep_last=2: keep new (id=4) + id=3; prune 2 and 1 newest-first.
        let pruned = nightly_releases_to_prune(&all, 2, 4, &unscoped("v1.2.3"));
        assert_eq!(
            pruned,
            vec![(2u64, "t2".to_string()), (1u64, "t1".to_string())],
            "must keep the highest-id releases regardless of input order",
        );
    }

    /// The cfgd shape: three tracks publishing under ONE rendered release
    /// name (`cfgd nightly`). Each track's sweep must prune only its own
    /// family — before family scoping, `cfgd`'s `keep_last: 1` sweep deleted
    /// the `operator-` and `csi-` releases created seconds earlier, and the
    /// git tags behind them, so `verify-release` 404'd on two of three tags.
    #[test]
    fn nightly_retention_prunes_only_the_publishing_track() {
        // Newest-first ids: this run created 6/5/4; the prior run left 3/2/1.
        let name_matched = vec![
            (6u64, "csi-v0.5.2-new-nightly".to_string()),
            (5u64, "operator-v0.5.2-new-nightly".to_string()),
            (4u64, "v0.5.2-new-nightly".to_string()),
            (3u64, "csi-v0.5.1-old-nightly".to_string()),
            (2u64, "operator-v0.5.1-old-nightly".to_string()),
            (1u64, "v0.5.1-old-nightly".to_string()),
        ];
        let cases = [
            (4u64, "v0.5.2-new-nightly", "v{{ Version }}", 1u64),
            (
                5u64,
                "operator-v0.5.2-new-nightly",
                "operator-v{{ Version }}",
                2u64,
            ),
            (6u64, "csi-v0.5.2-new-nightly", "csi-v{{ Version }}", 3u64),
        ];
        for (protect_id, tag, template, expected_pruned_id) in cases {
            let pruned =
                nightly_releases_to_prune(&name_matched, 1, protect_id, &family(tag, template));
            assert_eq!(
                pruned,
                vec![(
                    expected_pruned_id,
                    name_matched
                        .iter()
                        .find(|(id, _)| *id == expected_pruned_id)
                        .expect("fixture id")
                        .1
                        .clone()
                )],
                "track '{template}' must prune only its own family",
            );
        }
    }

    /// The empty-prefix alias: `v…-nightly` is a SUFFIX of
    /// `operator-v…-nightly`, so any contains/ends-with rule would let the
    /// `v` track swallow every sibling. The family matcher anchors on the
    /// template's literal prefix instead.
    #[test]
    fn nightly_retention_empty_prefix_family_excludes_prefixed_siblings() {
        let name_matched = vec![
            (4u64, "v0.5.2-new-nightly".to_string()),
            (3u64, "operator-v0.5.1-old-nightly".to_string()),
            (2u64, "csi-v0.5.1-old-nightly".to_string()),
            (1u64, "v0.5.1-old-nightly".to_string()),
        ];
        let pruned = nightly_releases_to_prune(
            &name_matched,
            1,
            4,
            &family("v0.5.2-new-nightly", "v{{ Version }}"),
        );
        assert_eq!(pruned, vec![(1u64, "v0.5.1-old-nightly".to_string())]);
    }

    /// A single-track workspace is unchanged by family scoping: the sweep
    /// still prunes every older nightly of the one family.
    #[test]
    fn nightly_retention_single_track_sweep_unchanged() {
        let name_matched = vec![
            (3u64, "v1.2.3-c-nightly".to_string()),
            (2u64, "v1.2.2-b-nightly".to_string()),
            (1u64, "v1.2.1-a-nightly".to_string()),
        ];
        assert_eq!(
            nightly_releases_to_prune(
                &name_matched,
                1,
                3,
                &single_track("v1.2.3-c-nightly", "v{{ Version }}")
            ),
            vec![
                (2u64, "v1.2.2-b-nightly".to_string()),
                (1u64, "v1.2.1-a-nightly".to_string()),
            ]
        );
    }

    /// A single-track repo that USED to pin `nightly.tag_name` carries older
    /// releases on a tag no family contains. Narrowing there would strand
    /// them forever; with one track the shared name is already unambiguous,
    /// so the sweep stays name-keyed and collects them.
    #[test]
    fn nightly_retention_single_track_still_prunes_a_legacy_rolling_tag() {
        let name_matched = vec![
            (3u64, "v1.2.3-c-nightly".to_string()),
            (2u64, "nightly".to_string()),
            (1u64, "nightly.0".to_string()),
        ];
        assert_eq!(
            nightly_releases_to_prune(
                &name_matched,
                1,
                3,
                &single_track("v1.2.3-c-nightly", "v{{ Version }}")
            ),
            vec![
                (2u64, "nightly".to_string()),
                (1u64, "nightly.0".to_string()),
            ]
        );
    }

    /// A literal `nightly.tag_name` mints a tag no family contains; the
    /// sweep then falls back to the name-only set it has always used.
    #[test]
    fn nightly_retention_falls_back_to_name_when_tag_is_outside_every_family() {
        let name_matched = vec![
            (2u64, "edge".to_string()),
            (1u64, "operator-v0.5.1-old-nightly".to_string()),
        ];
        let f = family("edge", "v{{ Version }}");
        assert!(!f.scopes());
        assert_eq!(
            nightly_releases_to_prune(&name_matched, 1, 2, &f),
            vec![(1u64, "operator-v0.5.1-old-nightly".to_string())]
        );
    }

    /// The scope line must name what the sweep EXCLUDES, not just what it
    /// matches: `v*` alone reads as every tag in the repository.
    #[test]
    fn retention_scope_line_names_the_excluded_sibling_families() {
        let siblings = vec![
            "operator-v{{ Version }}".to_string(),
            "csi-v{{ Version }}".to_string(),
        ];
        let family = NightlyRetentionFamily {
            tag: "v0.5.2-new-nightly",
            tag_template: "v{{ Version }}",
            sibling_templates: &siblings,
            monorepo_prefix: None,
            multitrack: true,
        };
        assert_eq!(family.describe(), "v* minus operator-v*, csi-v*");
    }

    /// A track with no narrower sibling has nothing to subtract, so the line
    /// stays the bare glob rather than growing an empty `minus` clause.
    #[test]
    fn retention_scope_line_omits_an_empty_exclusion_clause() {
        let siblings = vec!["v{{ Version }}".to_string()];
        let family = NightlyRetentionFamily {
            tag: "operator-v0.5.2-new-nightly",
            tag_template: "operator-v{{ Version }}",
            sibling_templates: &siblings,
            monorepo_prefix: None,
            multitrack: true,
        };
        assert_eq!(family.describe(), "operator-v*");
    }
}
