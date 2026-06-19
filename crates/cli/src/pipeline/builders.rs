//! Pipeline-construction functions for each entry point: full release,
//! split (build-only), publish, publish-only, announce-only, and merge.
//!
//! Each `build_*_pipeline` assembles a [`super::Pipeline`] by pushing the
//! stages for that command in dependency order. The ordering invariants
//! (blob before snapcraft-publish, sign before release in publish-only,
//! announce/verify terminal) are asserted by the tests at the foot of this
//! module.

use super::Pipeline;

/// Build the full release pipeline with all stages in order
pub fn build_release_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_appbundle::AppBundleStage;
    use anodizer_stage_appimage::AppImageStage;
    use anodizer_stage_archive::ArchiveStage;
    use anodizer_stage_attest::AttestStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_build::BuildStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_dmg::DmgStage;
    use anodizer_stage_docker::DockerStage;
    use anodizer_stage_flatpak::FlatpakStage;
    use anodizer_stage_makeself::MakeselfStage;
    use anodizer_stage_msi::MsiStage;
    use anodizer_stage_nfpm::NfpmStage;
    use anodizer_stage_notarize::NotarizeStage;
    use anodizer_stage_nsis::NsisStage;
    use anodizer_stage_pkg::PkgStage;
    use anodizer_stage_prepublish_guard::PrePublishGuardStage;
    use anodizer_stage_publish::{EmissionValidateStage, PublishStage};
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sbom::SbomStage;
    use anodizer_stage_sign::{DockerSignStage, SignStage};
    use anodizer_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodizer_stage_source::SourceStage;
    use anodizer_stage_srpm::SrpmStage;
    use anodizer_stage_templatefiles::TemplateFilesStage;
    use anodizer_stage_upx::UpxStage;
    use anodizer_stage_verify_release::VerifyReleaseStage;

    // Canonical stage order.
    // Anodizer-specific stages (appbundle, dmg, msi, pkg, nsis, templatefiles,
    // release, snapcraft-publish, blob) are interleaved at logical positions.
    let mut p = Pipeline::new();
    p.expect_binaries();

    // ── Per-crate lifecycle: before ────────────────────────────────────────
    // BeforeCrateStage runs each selected crate's `crates[].before` hooks at
    // the pipeline HEAD, after version/tag anchoring (done in the dispatcher
    // before `p.run`) and before the build. A full release runs as ONE
    // pipeline pass with no Rust-level per-crate loop, so without this stage
    // `crates[].before` would silently no-op on a workspace-per-crate /
    // lockstep-multi-crate config — even though `before_publish` and the
    // publishers DO iterate per crate. Publish-only fires before/after via its
    // own per-crate loop (`run_per_crate_lifecycle_hooks`) and deliberately
    // does NOT get these stages, so neither path double-fires.
    p.add(Box::new(anodizer_core::hooks::BeforeCrateStage));

    // ── Build ────────────────────────────────────────────────────────────
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    // AppBundle → DMG → PKG must run before Notarize (macOS signing).
    // MSI and NSIS are Windows equivalents at the same pipeline phase.
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(NotarizeStage));

    // ── Changelog ────────────────────────────────────────────────────────
    p.add(Box::new(ChangelogStage));

    // ── Packaging ────────────────────────────────────────────────────────
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SrpmStage));
    p.add(Box::new(MakeselfStage));
    p.add(Box::new(AppImageStage));
    p.add(Box::new(SnapcraftStage));
    p.add(Box::new(FlatpakStage));
    p.add(Box::new(SbomStage));
    p.add(Box::new(TemplateFilesStage));

    // ── Integrity ────────────────────────────────────────────────────────
    p.add(Box::new(ChecksumStage));
    // AttestStage runs after Checksum (so subject digests reuse the computed
    // sha256) and before Sign: in `emit` mode it registers the in-toto
    // statement as an UploadableFile, which the following SignStage then signs
    // and ReleaseStage uploads — no new signing path.
    p.add(Box::new(AttestStage));
    p.add(Box::new(SignStage));

    // ── Publish ──────────────────────────────────────────────────────────
    // EmissionValidateStage is a no-op in a real release; in snapshot/dry-run
    // it validates the binstall/nix/version-sync emissions (which the real
    // stages mutate/push but snapshot skips) against the produced asset set.
    // Runs after ChecksumStage so the archive cross-checks see every asset.
    p.add(Box::new(EmissionValidateStage));
    // BeforePublishStage runs user-defined `before_publish:` hooks here so a
    // non-zero hook can abort the release before any publisher writes to a
    // registry — last gate for smoke-tests / scanners against the staged dist.
    p.add(Box::new(anodizer_core::hooks::BeforePublishStage));
    p.add(Box::new(ReleaseStage));
    // PrePublishGuardStage runs immediately after ReleaseStage — once the
    // release exists, `ensure_release_url` has put the (real or derived)
    // `ReleaseURL` in ctx — and BEFORE any irreversible publisher
    // (chocolatey/winget moderation, AUR push) or announcer fires, so a broken
    // publisher-manifest or announce template aborts with no one-way door
    // already through.
    p.add(Box::new(PrePublishGuardStage));
    p.add(Box::new(DockerStage::new()));
    // DockerSignStage runs after DockerStage so docker image artifacts exist.
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    // BlobStage runs before SnapcraftPublishStage so a required-blob
    // failure can short-circuit the snapcraft upload via the same
    // `any_failed(Assets, required_only=true)` check that already gates
    // every other Submitter publisher.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(AnnounceStage));

    // ── Post-publish verification ────────────────────────────────────────
    // VerifyReleaseStage runs LAST — after the release exists and every
    // publisher has run — because it needs the published release to verify
    // against. A no-op unless `verify_release.enabled`; on a detected defect
    // it reports + exits non-zero but never undoes the (already-live) release.
    p.add(Box::new(VerifyReleaseStage));

    // ── Per-crate lifecycle: after ──────────────────────────────────────────
    // AfterCrateStage runs each selected crate's `crates[].after` hooks at the
    // pipeline TAIL — after publish + post-publish verification — mirroring the
    // top-level `after:` (which fires once after the whole release) and the
    // publish-only per-crate loop's `after` hooks. Tail counterpart of
    // BeforeCrateStage; same full-release-only placement (publish-only owns its
    // own per-crate after-hook firing).
    p.add(Box::new(anodizer_core::hooks::AfterCrateStage));
    p
}

/// Build a pipeline that only runs the build stage (for --split mode).
pub fn build_split_pipeline() -> Pipeline {
    use anodizer_stage_build::BuildStage;
    use anodizer_stage_upx::UpxStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    p
}

/// Build a publish-only pipeline: release, publish, blob, snapcraft-publish stages.
///
/// **Note**: this is the pipeline consumed by the LEGACY `anodize
/// publish` subcommand, which assumes the input dist was produced by
/// a full `anodize release` whose own SignStage already fired. Adding
/// a head SignStage here would silently introduce a new credential
/// requirement to the existing surface. The
/// `anodize release --publish-only` path uses
/// [`build_publish_only_pipeline`] instead, which DOES prepend
/// SignStage for the determinism-preserved-dist re-sign pass.
pub fn build_publish_pipeline() -> Pipeline {
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_prepublish_guard::PrePublishGuardStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_snapcraft::SnapcraftPublishStage;

    let mut p = Pipeline::new();
    // artifacts.json strips content-hash keys (sha256) for determinism, so a
    // from-disk publish must rehydrate them from the .sha256 sidecars before any
    // sha256-consuming publisher's schema-validate runs in PrePublishGuardStage.
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(anodizer_core::hooks::BeforePublishStage));
    p.add(Box::new(ReleaseStage));
    // Guard the (legacy) publish path too: a broken publisher-manifest or
    // announce template must abort after the release exists but before any
    // irreversible publisher fires.
    p.add(Box::new(PrePublishGuardStage));
    p.add(Box::new(PublishStage));
    // BlobStage before SnapcraftPublishStage so the snapcraft submitter
    // gate sees blob's outcome via `ctx.publish_report`.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p
}

/// Build the pipeline for `anodize release --publish-only`:
/// `[ChangelogStage, SignStage, ReleaseStage, PublishStage,
/// BlobStage, SnapcraftPublishStage, AnnounceStage]`. The head
/// `SignStage` is the production-keys re-sign pass — the preserved
/// dist's archive bytes are byte-stable (the determinism check
/// verified that) but their `.sig`/`.asc` signatures are either
/// missing entirely (harness skips Sign when prod keys are exported
/// on the runner) or ephemeral (harness ran without prod keys).
///
/// **Ordering invariants**:
/// - `ChangelogStage` runs first. It is a pure GitHub API call with
///   no artifact dependency, and `ReleaseStage::build_release_json`
///   reads `ctx.stage_outputs.changelogs` to populate the GitHub
///   release body — so it MUST land before `ReleaseStage`. Placing
///   it at the head also means a GitHub API failure aborts before
///   any signing work is performed.
/// - `AnnounceStage` runs last, matching `build_merge_pipeline` and
///   `build_release_pipeline`. The stage's internal
///   `required_publishers` gate then sees the final publish report
///   and only fires notifications on a green publish.
///
/// **Idempotence requirement on SignStage**: must be safe to re-run
/// on a dist whose existing `.sig`/`.asc` files are already
/// production signatures (gpg/cosign `--output` semantics overwrite
/// in place; `helpers::should_sign_artifact` excludes
/// `Signature`/`Certificate` artifact kinds from the `all`/`any`
/// filters so re-running can't produce `*.sig.sig` chains). The
/// publish-only entry point ALSO strips any *ephemeral* harness
/// signature/certificate artifacts up-front in
/// `commands/release/publish_only::strip_ephemeral_signatures` so
/// the head SignStage only sees the underlying archives.
///
/// Cross-platform packagers (msi/nsis/dmg/pkg/appbundle/flatpak/etc.)
/// that the harness's default stage list doesn't cover are expected
/// to have run in the upstream harness pipeline before preserve-dist
/// captured the tree — those stages are added to the harness's stage
/// list in CI and their outputs land under `dist/`. The publish-only
/// pipeline therefore consumes the full artifact set as-is and does
/// not re-run any artifact-producing stages.
pub(crate) fn build_publish_only_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_attest::AttestStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_docker::DockerStage;
    use anodizer_stage_prepublish_guard::PrePublishGuardStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sign::{DockerSignStage, SignStage};
    use anodizer_stage_snapcraft::SnapcraftPublishStage;
    use anodizer_stage_verify_release::VerifyReleaseStage;

    let mut p = Pipeline::new();
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(SignStage));
    // ChecksumStage between SignStage and PublishStage hashes the
    // production-signed bytes and backfills `sha256` onto every
    // artifact so each publisher sees the metadata its manifest
    // schema requires. The recompute is byte-deterministic, so this
    // is idempotent across re-runs.
    p.add(Box::new(ChecksumStage));
    // AttestStage re-derives the subjects manifest from the recomputed
    // digests and re-registers the emit-mode in-toto statement (byte-stable,
    // so its preserved upstream signature still matches) so ReleaseStage
    // uploads both. The emit-mode statement is signed in the upstream harness
    // run that produced the preserved dist; no re-sign happens here.
    p.add(Box::new(AttestStage));
    p.add(Box::new(anodizer_core::hooks::BeforePublishStage));
    p.add(Box::new(ReleaseStage));
    // Abort before any irreversible publisher / announcer if a manifest or
    // announce template fails to render — the release exists by now, so
    // `ReleaseURL` is in ctx for the announce dry-render.
    p.add(Box::new(PrePublishGuardStage));
    // Docker build+sign land between the GitHub release and PublishStage:
    // the mcp publisher (inside PublishStage) validates that the OCI image
    // its manifest references already exists in the registry, so the image
    // must be built and pushed first.
    p.add(Box::new(DockerStage::new()));
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(AnnounceStage));
    // Post-publish verification runs LAST here too: `release --publish-only`
    // creates a real release + publishes, so the same gate applies.
    p.add(Box::new(VerifyReleaseStage));
    p
}

/// Build an announce-only pipeline.
pub fn build_announce_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;

    let mut p = Pipeline::new();
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline for --merge mode: all post-build stages.
pub fn build_merge_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_appbundle::AppBundleStage;
    use anodizer_stage_appimage::AppImageStage;
    use anodizer_stage_archive::ArchiveStage;
    use anodizer_stage_attest::AttestStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_dmg::DmgStage;
    use anodizer_stage_docker::DockerStage;
    use anodizer_stage_flatpak::FlatpakStage;
    use anodizer_stage_makeself::MakeselfStage;
    use anodizer_stage_msi::MsiStage;
    use anodizer_stage_nfpm::NfpmStage;
    use anodizer_stage_notarize::NotarizeStage;
    use anodizer_stage_nsis::NsisStage;
    use anodizer_stage_pkg::PkgStage;
    use anodizer_stage_prepublish_guard::PrePublishGuardStage;
    use anodizer_stage_publish::{EmissionValidateStage, PublishStage};
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sbom::SbomStage;
    use anodizer_stage_sign::{DockerSignStage, SignStage};
    use anodizer_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodizer_stage_source::SourceStage;
    use anodizer_stage_srpm::SrpmStage;
    use anodizer_stage_templatefiles::TemplateFilesStage;
    use anodizer_stage_verify_release::VerifyReleaseStage;

    // Merge pipeline: same order as build_release_pipeline minus Build/UPX.
    let mut p = Pipeline::new();
    p.expect_binaries();
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(NotarizeStage));
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SrpmStage));
    p.add(Box::new(MakeselfStage));
    p.add(Box::new(AppImageStage));
    p.add(Box::new(SnapcraftStage));
    p.add(Box::new(FlatpakStage));
    p.add(Box::new(SbomStage));
    p.add(Box::new(TemplateFilesStage));
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(AttestStage));
    p.add(Box::new(SignStage));
    // Snapshot/dry-run emission validation; no-op in a real release.
    p.add(Box::new(EmissionValidateStage));
    p.add(Box::new(anodizer_core::hooks::BeforePublishStage));
    p.add(Box::new(ReleaseStage));
    // Same one-way-door guard as build_release_pipeline: abort on a broken
    // publisher-manifest or announce template before any irreversible
    // publisher fires, with `ReleaseURL` already in ctx post-Release.
    p.add(Box::new(PrePublishGuardStage));
    p.add(Box::new(DockerStage::new()));
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    // BlobStage before SnapcraftPublishStage — mirrors
    // `build_release_pipeline`'s swap so merge-mode runs share the same
    // submitter-gate semantics.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(AnnounceStage));
    // Merge mode produces + publishes a real release, so the post-publish
    // gate runs last here too.
    p.add(Box::new(VerifyReleaseStage));
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // sh -c mangles backslashes; feed it a forward-slash path so the redirect
    // target resolves on Windows (no-op on Linux where the path has none).
    fn sh_path(p: &std::path::Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    /// `Pipeline::run` ends with a default summary write to
    /// `<dist>/run-<id>/summary.json`; with the default relative
    /// `./dist` and the crate root as test cwd that would land in the
    /// working tree. Point `dist` at a tempdir; the returned guard
    /// keeps it alive across the run.
    fn isolate_dist(ctx: &mut anodizer_core::context::Context) -> TempDir {
        let tmp = TempDir::new().expect("tempdir");
        ctx.config.dist = tmp.path().to_path_buf();
        tmp
    }

    // -----------------------------------------------------------------------
    // Stage-order invariants
    //
    // BlobStage must run BEFORE SnapcraftPublishStage in every pipeline
    // variant so a required-blob failure can short-circuit the
    // (irreversible) snapcraft upload via the same
    // `any_failed(Assets, required_only=true)` gate that already
    // protects every other Submitter publisher.
    // -----------------------------------------------------------------------

    fn assert_blob_before_snapcraft(names: &[&str], pipeline: &str) {
        let blob_idx = names
            .iter()
            .position(|n| *n == "blob")
            .unwrap_or_else(|| panic!("{pipeline}: missing blob stage; got {names:?}"));
        let snap_idx = names
            .iter()
            .position(|n| *n == "snapcraft-publish")
            .unwrap_or_else(|| panic!("{pipeline}: missing snapcraft-publish; got {names:?}"));
        assert!(
            blob_idx < snap_idx,
            "{pipeline}: blob (idx {blob_idx}) must precede snapcraft-publish (idx {snap_idx}); got {names:?}"
        );
    }

    #[test]
    fn release_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_release_pipeline");
    }

    // -----------------------------------------------------------------------
    // PrePublishGuardStage ordering
    //
    // The guard must sit AFTER ReleaseStage (so `ReleaseURL` is in ctx for the
    // announce dry-render) and BEFORE every irreversible publisher
    // (PublishStage, SnapcraftPublishStage) and before DockerStage, so a broken
    // publisher-manifest or announce template aborts with no one-way door
    // already through.
    // -----------------------------------------------------------------------

    fn idx(names: &[&str], stage: &str, pipeline: &str) -> usize {
        names
            .iter()
            .position(|n| *n == stage)
            .unwrap_or_else(|| panic!("{pipeline}: missing {stage} stage; got {names:?}"))
    }

    fn assert_guard_after_release_before_publishers(names: &[&str], pipeline: &str) {
        let release = idx(names, "release", pipeline);
        let guard = idx(names, "prepublish-guard", pipeline);
        let publish = idx(names, "publish", pipeline);
        assert!(
            release < guard,
            "{pipeline}: release ({release}) must precede prepublish-guard ({guard}); {names:?}"
        );
        assert!(
            guard < publish,
            "{pipeline}: prepublish-guard ({guard}) must precede publish ({publish}); {names:?}"
        );
        // Docker and snapcraft-publish are present only in some pipelines; when
        // present they must follow the guard (docker pushes an image; snapcraft
        // uploads to the store — both fire publishers/registries).
        if let Some(docker) = names.iter().position(|n| *n == "docker") {
            assert!(
                guard < docker,
                "{pipeline}: prepublish-guard ({guard}) must precede docker ({docker}); {names:?}"
            );
        }
        if let Some(snap) = names.iter().position(|n| *n == "snapcraft-publish") {
            assert!(
                guard < snap,
                "{pipeline}: prepublish-guard ({guard}) must precede snapcraft-publish ({snap}); {names:?}"
            );
        }
    }

    #[test]
    fn release_pipeline_runs_guard_after_release_before_publishers() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        assert_guard_after_release_before_publishers(&names, "build_release_pipeline");
    }

    #[test]
    fn merge_pipeline_runs_guard_after_release_before_publishers() {
        let p = build_merge_pipeline();
        let names = p.stage_names();
        assert_guard_after_release_before_publishers(&names, "build_merge_pipeline");
    }

    #[test]
    fn publish_pipeline_runs_guard_after_release_before_publishers() {
        let p = build_publish_pipeline();
        let names = p.stage_names();
        assert_guard_after_release_before_publishers(&names, "build_publish_pipeline");
    }

    #[test]
    fn publish_only_pipeline_runs_guard_after_release_before_publishers() {
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        assert_guard_after_release_before_publishers(&names, "build_publish_only_pipeline");
    }

    /// Stage order: ChecksumStage → PrePublishGuardStage.
    /// `build_publish_pipeline` loads a serialized dist whose
    /// artifacts.json had `sha256` stripped for determinism;
    /// ChecksumStage must re-run to rehydrate the per-artifact
    /// `sha256` from the `.sha256` sidecars before the guard's
    /// schema-validate (which sha256-consuming publishers —
    /// aur/krew/homebrew/scoop/nix/npm — require) runs.
    #[test]
    fn publish_pipeline_runs_checksum_before_guard() {
        let p = build_publish_pipeline();
        let names = p.stage_names();
        let checksum_idx = idx(&names, "checksum", "build_publish_pipeline");
        let guard_idx = idx(&names, "prepublish-guard", "build_publish_pipeline");
        assert!(
            checksum_idx < guard_idx,
            "checksum (idx {checksum_idx}) must precede prepublish-guard (idx {guard_idx}) so publishers see sha256 metadata; got {names:?}"
        );
    }

    #[test]
    fn publish_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_publish_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_publish_pipeline");
    }

    #[test]
    fn merge_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_merge_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_merge_pipeline");
    }

    #[test]
    fn publish_only_pipeline_runs_blob_before_snapcraft_publish() {
        // The `--publish-only` pipeline must honor the same
        // blob-before-snapcraft-publish ordering as every other
        // variant so a required-blob failure can short-circuit the
        // (irreversible) snapcraft upload via the
        // `any_failed(Assets, required_only=true)` gate.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_publish_only_pipeline");
    }

    #[test]
    fn publish_only_pipeline_runs_sign_before_release() {
        // SignStage must be at the HEAD of the publish-only pipeline
        // so production signatures land on the preserved archives
        // BEFORE ReleaseStage uploads them.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let sign_idx = names
            .iter()
            .position(|n| *n == "sign")
            .expect("publish-only pipeline must include sign stage");
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("publish-only pipeline must include release stage");
        assert!(
            sign_idx < release_idx,
            "sign (idx {sign_idx}) must precede release (idx {release_idx}); got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_runs_docker_after_release_before_publish() {
        // Docker build+sign must sit between the GitHub release and the
        // publish stage: the mcp publisher (inside PublishStage) validates
        // that the OCI image its manifest references already exists, so the
        // image must be built and pushed before publish runs.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("publish-only pipeline must include release stage");
        let docker_idx = names
            .iter()
            .position(|n| *n == "docker")
            .expect("publish-only pipeline must include docker stage");
        let docker_sign_idx = names
            .iter()
            .position(|n| *n == "docker-sign")
            .expect("publish-only pipeline must include docker-sign stage");
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .expect("publish-only pipeline must include publish stage");
        assert!(
            release_idx < docker_idx,
            "docker (idx {docker_idx}) must follow release (idx {release_idx}); got {names:?}"
        );
        assert!(
            docker_idx < docker_sign_idx,
            "docker-sign (idx {docker_sign_idx}) must follow docker (idx {docker_idx}); got {names:?}"
        );
        assert!(
            docker_sign_idx < publish_idx,
            "docker-sign (idx {docker_sign_idx}) must precede publish (idx {publish_idx}); got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_runs_changelog_before_release() {
        // ReleaseStage::build_release_json reads ctx.stage_outputs.changelogs;
        // without ChangelogStage ahead of it the GitHub release body would
        // be empty even though the project configures `changelog.use:
        // github-native`. ChangelogStage at the head also costs no signing
        // work if its GitHub API call fails.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let changelog_idx = names
            .iter()
            .position(|n| *n == "changelog")
            .expect("publish-only pipeline must include changelog stage");
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("publish-only pipeline must include release stage");
        assert!(
            changelog_idx < release_idx,
            "changelog (idx {changelog_idx}) must precede release (idx {release_idx}); got {names:?}"
        );
    }

    /// Stage order: SignStage → ChecksumStage → PublishStage.
    /// ChecksumStage must follow Sign so signed bytes are what get
    /// hashed, and must precede Publish so every publisher
    /// (winget, chocolatey, scoop, krew, …) sees per-artifact
    /// `sha256` metadata its manifest schema requires.
    #[test]
    fn publish_only_pipeline_runs_checksum_before_publish_after_sign() {
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let checksum_idx = names
            .iter()
            .position(|n| *n == "checksum")
            .expect("publish-only pipeline must include checksum stage");
        let sign_idx = names
            .iter()
            .position(|n| *n == "sign")
            .expect("publish-only pipeline must include sign stage");
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .expect("publish-only pipeline must include publish stage");
        assert!(
            sign_idx < checksum_idx,
            "checksum (idx {checksum_idx}) must follow sign (idx {sign_idx}) so production-signed bytes get hashed; got {names:?}"
        );
        assert!(
            checksum_idx < publish_idx,
            "checksum (idx {checksum_idx}) must precede publish (idx {publish_idx}) so publishers see sha256 metadata; got {names:?}"
        );
    }

    /// Assert the terminal-stage invariant shared by every publishing
    /// pipeline (release / merge / publish-only): AnnounceStage follows the
    /// publisher chain so it only fires on a green release and the
    /// `required_publishers` gate sees the final publish report, and it is the
    /// last stage of the *publish phase* — immediately before the
    /// `verify-release` post-publish report. `verify-release` runs AFTER
    /// announce (it needs the live release to verify against).
    ///
    /// `verify-release` is the absolute terminal stage EXCEPT in the full
    /// release pipeline, which appends one trailing per-crate `after`
    /// lifecycle stage (the tail counterpart of `BeforeCrateStage`). The
    /// `after` hooks must fire after the whole release completes — matching
    /// the top-level `after:`, which fires last in `run_post_pipeline` — so
    /// verify-release is the last *post-publish report* stage, with `after`
    /// (when present) trailing it. publish-only / merge carry no `after`
    /// stage (they fire per-crate / top-level after-hooks elsewhere), so for
    /// them verify-release is the absolute final stage.
    fn assert_announce_then_verify_release_terminal(names: &[&str], label: &str) {
        let announce_idx = names
            .iter()
            .position(|n| *n == "announce")
            .unwrap_or_else(|| panic!("{label} must include announce stage"));
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .unwrap_or_else(|| panic!("{label} must include publish stage"));
        let verify_release_idx = names
            .iter()
            .position(|n| *n == "verify-release")
            .unwrap_or_else(|| panic!("{label} must include verify-release stage"));
        assert!(
            announce_idx > publish_idx,
            "{label}: announce (idx {announce_idx}) must follow publish (idx {publish_idx}); got {names:?}"
        );
        // verify-release is terminal among the report stages; the only stage
        // allowed to trail it is the per-crate `after` lifecycle hook stage.
        let after_idx = names.iter().position(|n| *n == "after");
        let expected_last = match after_idx {
            Some(after) => {
                assert_eq!(
                    after,
                    names.len() - 1,
                    "{label}: `after` lifecycle stage, when present, must be the absolute terminal stage; got {names:?}"
                );
                assert!(
                    after > verify_release_idx,
                    "{label}: `after` ({after}) must trail verify-release ({verify_release_idx}); got {names:?}"
                );
                names.len() - 2
            }
            None => names.len() - 1,
        };
        assert_eq!(
            verify_release_idx, expected_last,
            "{label}: verify-release must be the terminal post-publish report stage (before any trailing `after`); got {names:?}"
        );
        assert_eq!(
            announce_idx,
            verify_release_idx - 1,
            "{label}: announce must be the final publish-phase stage, immediately before the terminal verify-release report; got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_runs_announce_after_publish() {
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        assert_announce_then_verify_release_terminal(&names, "build_publish_only_pipeline");
    }

    #[test]
    fn release_pipeline_runs_announce_after_publish() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        assert_announce_then_verify_release_terminal(&names, "build_release_pipeline");
    }

    #[test]
    fn merge_pipeline_runs_announce_after_publish() {
        let p = build_merge_pipeline();
        let names = p.stage_names();
        assert_announce_then_verify_release_terminal(&names, "build_merge_pipeline");
    }

    // -----------------------------------------------------------------------
    // before_publish: hooks
    // -----------------------------------------------------------------------

    /// Register a single sentinel archive artifact on the context. The
    /// before-publish stage runs once per matching artifact, so any test
    /// that asserts the hook executed (rather than asserting it didn't
    /// because of a filter / `if:` gate / dry-run) must seed at least
    /// one artifact for the per-artifact iteration to fire against.
    fn add_sentinel_archive(ctx: &mut anodizer_core::context::Context) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/myapp_linux_amd64.tar.gz"),
            name: "myapp_linux_amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    /// `release` pipeline: BeforePublishStage runs AFTER sign/checksum (the
    /// integrity stages) and BEFORE release/publish (the publish phase),
    /// so a non-zero hook can abort the release before any publisher writes
    /// to a registry.
    #[test]
    fn before_publish_runs_after_sbom_before_publish_dispatch() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        let sbom_idx = names
            .iter()
            .position(|n| *n == "sbom")
            .expect("release pipeline must include sbom stage");
        let sign_idx = names
            .iter()
            .position(|n| *n == "sign")
            .expect("release pipeline must include sign stage");
        let checksum_idx = names
            .iter()
            .position(|n| *n == "checksum")
            .expect("release pipeline must include checksum stage");
        let before_publish_idx = names
            .iter()
            .position(|n| *n == "before-publish")
            .expect("release pipeline must include before-publish stage");
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("release pipeline must include release stage");
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .expect("release pipeline must include publish stage");

        assert!(
            sbom_idx < before_publish_idx,
            "before-publish ({before_publish_idx}) must follow sbom ({sbom_idx}); got {names:?}"
        );
        assert!(
            sign_idx < before_publish_idx,
            "before-publish ({before_publish_idx}) must follow sign ({sign_idx}); got {names:?}"
        );
        assert!(
            checksum_idx < before_publish_idx,
            "before-publish ({before_publish_idx}) must follow checksum ({checksum_idx}); got {names:?}"
        );
        assert!(
            before_publish_idx < release_idx,
            "before-publish ({before_publish_idx}) must precede release ({release_idx}); got {names:?}"
        );
        assert!(
            before_publish_idx < publish_idx,
            "before-publish ({before_publish_idx}) must precede publish ({publish_idx}); got {names:?}"
        );
    }

    /// A hook exiting non-zero must surface as Err from the pipeline so the
    /// PublishStage never gets to dispatch. Verified by building a pipeline
    /// of `[BeforePublishStage, RecordingStage]` and asserting that
    /// RecordingStage never ran.
    #[test]
    fn before_publish_hook_failure_aborts_release_before_publish_dispatch() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordingStage(Arc<AtomicBool>);
        impl anodizer_core::stage::Stage for RecordingStage {
            fn name(&self) -> &str {
                "publish"
            }
            fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let publish_ran = Arc::new(AtomicBool::new(false));

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));
        p.add(Box::new(RecordingStage(publish_ran.clone())));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: "exit 1".to_string(),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        add_sentinel_archive(&mut ctx);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);

        assert!(
            result.is_err(),
            "non-zero before_publish hook must abort the pipeline; got Ok",
        );
        assert!(
            !publish_ran.load(Ordering::SeqCst),
            "publish stage must NOT run after a failed before_publish hook",
        );
    }

    /// `--skip=before-publish` short-circuits the stage (the pipeline's
    /// generic skip handling fires before stage.run is invoked) AND lets
    /// every subsequent stage continue.
    #[test]
    fn before_publish_skip_via_cli_flag_logs_and_continues() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct SentinelStage(Arc<AtomicBool>);
        impl anodizer_core::stage::Stage for SentinelStage {
            fn name(&self) -> &str {
                "publish"
            }
            fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let publish_ran = Arc::new(AtomicBool::new(false));

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));
        p.add(Box::new(SentinelStage(publish_ran.clone())));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        // Configure a hook that would FAIL — `--skip` must prevent it from
        // running so subsequent stages still execute.
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: "exit 1".to_string(),
                ..Default::default()
            })]),
            post: None,
        });

        let opts = ContextOptions {
            skip_stages: vec!["before-publish".to_string()],
            ..ContextOptions::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("pipeline must succeed when before-publish is skipped");

        assert!(
            publish_ran.load(Ordering::SeqCst),
            "publish stage must run when before-publish is operator-skipped",
        );
    }

    /// Dry-run shape: the hook runner logs `(dry-run) would run before-publish hook ...`
    /// instead of spawning the subprocess. Verified by asking the stage to
    /// run with a `exit 1` hook under dry-run; if the subprocess actually
    /// fired the pipeline would Err.
    #[test]
    fn before_publish_skip_via_cli_flag_via_dry_run() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: "exit 1".to_string(),
                ..Default::default()
            })]),
            post: None,
        });

        let opts = ContextOptions {
            dry_run: true,
            ..ContextOptions::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        add_sentinel_archive(&mut ctx);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("dry-run before_publish hook must NOT execute the subprocess");
    }

    /// `if: "{{ IsSnapshot }}"` skips when not a snapshot. Mirrors the shared
    /// `evaluate_if_condition` behavior exercised by build / archive / sign
    /// hooks — pinning the contract for before-publish too.
    #[test]
    fn before_publish_hook_if_condition_skip_when_falsy() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: "exit 1".to_string(),
                if_condition: Some("{{ IsSnapshot }}".to_string()),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        add_sentinel_archive(&mut ctx);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("falsy `if:` must skip the hook so the exit-1 cmd never spawns");
    }

    /// `output: true` streams stdout to the StageLogger so operators see
    /// hook progress in real time. Verified by capturing tracing output.
    #[test]
    fn before_publish_hook_output_true_streams_logs() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: "echo hello-from-before-publish".to_string(),
                output: Some(true),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        add_sentinel_archive(&mut ctx);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        // The subprocess returns 0 and prints to stdout — the run must succeed.
        // `output: true` plumbing is identical to the shared `run_hooks` path
        // already exercised by `crates/core/src/hooks.rs::tests`; this test
        // pins the call site, not the output capture mechanism itself.
        p.run(&mut ctx, &log)
            .expect("echo hook must succeed under before-publish");
    }

    /// Per-hook `env:` propagates to the subprocess. Verified by running a
    /// hook whose cmd asserts `$FOO == bar` — exits non-zero if the env var
    /// is not visible.
    #[test]
    fn before_publish_hook_env_propagates() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: r#"sh -c 'test "$FOO" = "bar"'"#.to_string(),
                env: Some(vec!["FOO=bar".to_string()]),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        add_sentinel_archive(&mut ctx);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("per-hook env must reach the subprocess");
    }

    /// Shorthand form `before_publish: { hooks: ["echo foo"] }` parses as a
    /// `HookEntry::Simple` (same shape as top-level `before:` / `after:`).
    #[test]
    fn before_publish_string_form_parses() {
        use anodizer_core::config::{Config, HookEntry};

        let yaml = r#"
project_name: myapp
crates:
  - name: myapp
    path: ""
before_publish:
  hooks:
    - "echo foo"
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse yaml");
        let hooks = cfg
            .before_publish
            .as_ref()
            .expect("before_publish set")
            .hooks
            .as_ref()
            .expect("hooks set");
        assert_eq!(hooks.len(), 1);
        match &hooks[0] {
            HookEntry::Simple(s) => assert_eq!(s, "echo foo"),
            HookEntry::Structured(h) => panic!("expected Simple, got Structured({:?})", h),
        }
    }

    // -----------------------------------------------------------------------
    // before_publish per-artifact iteration
    // -----------------------------------------------------------------------

    /// Register N archives, one hook with `artifacts: archive`, and verify
    /// the rendered cmd carried each artifact's `ArtifactPath` exactly once.
    /// The hook writes one line per invocation into a tempfile so the test
    /// can count by reading the file back.
    #[test]
    fn before_publish_runs_per_matching_artifact() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{
            BeforePublishArtifactFilter, HookEntry, HooksConfig, StructuredHook,
        };
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("hook-invocations.log");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!("echo {{{{ ArtifactPath }}}} >> {}", sh_path(&log_path)),
                artifacts: Some(BeforePublishArtifactFilter::Archive),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        for i in 0..3 {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from(format!("dist/myapp_{i}.tar.gz")),
                name: format!("myapp_{i}.tar.gz"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("per-artifact iteration must succeed");

        let contents = fs::read_to_string(&log_path).expect("log file exists");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "hook should run 3 times, got: {lines:?}");
        for i in 0..3 {
            let expected = format!("dist/myapp_{i}.tar.gz");
            assert!(
                lines.iter().any(|l| l == &expected),
                "missing iteration for {expected}; got {lines:?}"
            );
        }
    }

    /// `ids: [a]` restricts iteration to artifacts whose `metadata["id"] == "a"`.
    /// Register two archives with ids `a` and `b`; only `a` should fire.
    #[test]
    fn before_publish_ids_filter_narrows_to_subset() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("ids-filter.log");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!("echo {{{{ ArtifactID }}}} >> {}", sh_path(&log_path)),
                ids: Some(vec!["a".to_string()]),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        for id in &["a", "b"] {
            let mut meta = HashMap::new();
            meta.insert("id".to_string(), (*id).to_string());
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from(format!("dist/myapp-{id}.tar.gz")),
                name: format!("myapp-{id}.tar.gz"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: meta,
                size: None,
            });
        }

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log).expect("ids filter must not error");

        let contents = fs::read_to_string(&log_path).expect("log file exists");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["a"], "only id=a should match; got {lines:?}");
    }

    /// `artifacts: archive` excludes a binary artifact: register one binary
    /// and one archive, then verify only the archive triggered the hook.
    #[test]
    fn before_publish_artifacts_filter_excludes_non_matching_kinds() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{
            BeforePublishArtifactFilter, HookEntry, HooksConfig, StructuredHook,
        };
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("kind-filter.log");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!(
                    "echo {{{{ ArtifactKind }}}}={{{{ ArtifactName }}}} >> {}",
                    sh_path(&log_path)
                ),
                artifacts: Some(BeforePublishArtifactFilter::Archive),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from("dist/myapp"),
            name: "myapp".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/myapp.tar.gz"),
            name: "myapp.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("archive filter must not error");

        let contents = fs::read_to_string(&log_path).expect("log file exists");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines,
            vec!["archive=myapp.tar.gz"],
            "archive filter must skip binary; got {lines:?}"
        );
    }

    /// Per-artifact template variables (`ArtifactPath`, `ArtifactName`,
    /// `ArtifactExt`, `Os`, `Arch`) all render correctly for each
    /// iteration.
    #[test]
    fn before_publish_template_artifact_vars_bound() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("vars.log");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        // Each `{{ Var }}` renders to a token; the cmd writes them
        // space-separated onto one line. The pipe character is
        // deliberately avoided (it has shell meaning under `sh -c`).
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!(
                    "printf '%s %s %s %s %s\\n' {{{{ ArtifactPath }}}} {{{{ ArtifactName }}}} {{{{ ArtifactExt }}}} {{{{ Os }}}} {{{{ Arch }}}} >> {}",
                    sh_path(&log_path)
                ),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/myapp_linux_amd64.tar.gz"),
            name: "myapp_linux_amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("template-vars hook must succeed");

        let contents = fs::read_to_string(&log_path).expect("log file exists");
        let line = contents.lines().next().expect("at least one line").trim();
        assert_eq!(
            line, "dist/myapp_linux_amd64.tar.gz myapp_linux_amd64.tar.gz .tar.gz linux amd64",
            "all per-artifact template vars must bind; got {line:?}"
        );
    }

    /// A hook command that exits non-zero on the second artifact aborts the
    /// pipeline so the publish stage never dispatches. The cmd writes its
    /// own iteration count to disk and exits 1 once it sees two
    /// invocations.
    #[test]
    fn before_publish_failure_on_any_artifact_aborts_release() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordingStage(Arc<AtomicBool>);
        impl anodizer_core::stage::Stage for RecordingStage {
            fn name(&self) -> &str {
                "publish"
            }
            fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let publish_ran = Arc::new(AtomicBool::new(false));
        let tmp = TempDir::new().unwrap();
        let counter_path = tmp.path().join("counter");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));
        p.add(Box::new(RecordingStage(publish_ran.clone())));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        // The cmd appends a byte per invocation; when the file size reaches
        // 2, it exits 1 — so the second artifact's iteration fails.
        let cmd = format!(
            r#"sh -c 'printf x >> {p}; if [ "$(wc -c < {p})" -ge 2 ]; then exit 1; fi'"#,
            p = sh_path(&counter_path),
        );
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd,
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        for i in 0..3 {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from(format!("dist/myapp_{i}.tar.gz")),
                name: format!("myapp_{i}.tar.gz"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);
        assert!(
            result.is_err(),
            "hook failure on any artifact must abort the pipeline",
        );
        assert!(
            !publish_ran.load(Ordering::SeqCst),
            "publish stage must NOT run after a mid-iteration hook failure",
        );
        let count = fs::read_to_string(&counter_path)
            .map(|s| s.len())
            .unwrap_or(0);
        assert_eq!(
            count, 2,
            "hook should have run exactly twice before aborting; got {count}",
        );
    }

    /// Omitting `artifacts:` is equivalent to `all`: the hook fires against
    /// every registered artifact regardless of kind.
    #[test]
    fn before_publish_artifacts_all_default_matches_everything() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        use anodizer_core::context::ContextOptions;
        use anodizer_core::hooks::BeforePublishStage;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("default-all.log");

        let mut p = Pipeline::new();
        p.add(Box::new(BeforePublishStage));

        let mut config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        config.before_publish = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!("echo {{{{ ArtifactKind }}}} >> {}", sh_path(&log_path)),
                ..Default::default()
            })]),
            post: None,
        });
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");

        let kinds = [
            ArtifactKind::Binary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::Sbom,
        ];
        for (i, kind) in kinds.iter().enumerate() {
            ctx.artifacts.add(Artifact {
                kind: *kind,
                path: PathBuf::from(format!("dist/a{i}")),
                name: format!("a{i}"),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("default-all filter must fire for every artifact");

        let contents = fs::read_to_string(&log_path).expect("log file exists");
        let mut lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        lines.sort();
        assert_eq!(
            lines,
            vec!["archive", "binary", "checksum", "sbom"],
            "default (artifacts: all) must match every kind; got {lines:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline-level emit_summary contract.
    //
    // Pipeline::run must ALWAYS invoke `emit_summary` (regardless of
    // whether `AnnounceStage::run` was reached). The unit tests in
    // `stage-announce` pin the stage-side contract; this test pins the
    // pipeline-side contract — specifically that `--skip=announce`
    // doesn't drop `--summary-json`.
    // -----------------------------------------------------------------------

    /// A `Stage` that always returns `Err`. Pins the "emit_summary
    /// fires even on inner-fn Err" half of `Pipeline::run`'s contract.
    /// Kept private to the test module.
    struct AlwaysFailStage;
    impl anodizer_core::stage::Stage for AlwaysFailStage {
        fn name(&self) -> &str {
            "always-fail"
        }
        fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
            anyhow::bail!("synthetic stage failure for the I-3 test")
        }
    }

    #[test]
    fn pipeline_emits_summary_even_when_inner_stage_returns_err() {
        // The inner-fn scope-guard shape in `Pipeline::run` must
        // invoke `emit_summary` on Err too, not just on Ok. Without
        // this test, only the doc line pinned the contract; this
        // puts a bisectable green/red signal on the Err path.
        use anodizer_core::context::ContextOptions;

        let tmp = TempDir::new().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        let mut p = Pipeline::new();
        p.add(Box::new(AlwaysFailStage));

        let opts = ContextOptions {
            summary_json_path: Some(summary_path.clone()),
            ..ContextOptions::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        ctx.publish_report = Some(anodizer_core::publish_report::PublishReport::default());

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);

        assert!(
            result.is_err(),
            "pipeline must propagate the stage's Err verbatim",
        );
        assert!(
            summary_path.exists(),
            "summary.json must be written even when the inner pipeline body returns Err",
        );
    }

    #[test]
    fn pipeline_writes_default_summary_with_publish_state_on_post_publish_failure() {
        // The 2026-06-11 v0.8.0 incident: `release --publish-only` ran
        // every publisher to success, then the verify-release stage
        // failed — and NO summary.json landed on disk because nothing
        // passed `--summary-json`. CI then had no machine-readable
        // publish state and rolled back a fully-published release.
        // Pin the fix: without an explicit path, a failing pipeline
        // still writes `<dist>/run-<id>/summary.json` carrying the
        // per-publisher outcomes and the top-level publish counts.
        use anodizer_core::context::ContextOptions;
        use anodizer_core::publish_report::{
            PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
        };

        struct FailingVerifyStage;
        impl anodizer_core::stage::Stage for FailingVerifyStage {
            fn name(&self) -> &str {
                "verify-release"
            }
            fn run(&self, ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
                // Mirror the real stage: stamp the verdict BEFORE bailing so
                // the pipeline-end summary records it.
                ctx.verify_release = Some(anodizer_core::VerifyReleaseSummary {
                    issues: vec!["install smoke-test failed for crate 'myapp'".to_string()],
                });
                anyhow::bail!("post-publish verification found issues")
            }
        }

        let mut p = Pipeline::new();
        p.add(Box::new(FailingVerifyStage));

        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        ctx.publish_report = Some(report);

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);
        assert!(result.is_err(), "verify-release failure must propagate");

        // No git info in the fixture → derive_run_id falls back to "local".
        let summary_path = ctx.config.dist.join("run-local").join("summary.json");
        assert!(
            summary_path.exists(),
            "default <dist>/run-<id>/summary.json must be written on stage failure"
        );
        let parsed: anodizer_stage_publish::run_summary::RunSummary =
            serde_json::from_str(&fs::read_to_string(&summary_path).expect("read summary"))
                .expect("parse summary");
        assert_eq!(parsed.publishers_succeeded, 1);
        assert_eq!(parsed.publishers_failed, 0);
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].status, "succeeded");
        assert!(
            parsed.irreversibly_published,
            "a landed Submitter (cargo) must mark the version burned"
        );
        // The publish landed, but verify-release failed: the summary must
        // record the defect on its own axis (not a false all-green).
        let vr = parsed
            .verify_release
            .expect("a failing verify-release stage must record its verdict");
        assert!(!vr.passed, "verify-release defect => passed == false");
        assert_eq!(vr.issue_count, 1);
    }

    #[test]
    fn pipeline_writes_default_summary_on_release_stage_failure_before_publish() {
        // The 2026-06-11 v0.9.0 incident: the RELEASE stage failed on an
        // asset upload AFTER the GitHub release was created, the publish
        // stage never ran (publish_report = None), and no summary.json
        // landed on disk — CI's "Upload run summary" step found nothing
        // and recovery had no machine-readable state. Pin the fix: any
        // real (non-snapshot) pipeline failure after tag resolution
        // writes the default summary, carrying the tag, an EMPTY
        // publisher table, and irreversibly_published: false.
        use anodizer_core::context::ContextOptions;

        struct FailingReleaseStage;
        impl anodizer_core::stage::Stage for FailingReleaseStage {
            fn name(&self) -> &str {
                "release"
            }
            fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
                anyhow::bail!("release: upload artifact 'x.tar.zst.sha256' to release 'v9.9.9'")
            }
        }

        let mut p = Pipeline::new();
        p.add(Box::new(FailingReleaseStage));

        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        // The release stage failed before publish: NO publish_report.
        assert!(ctx.publish_report.is_none());

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);
        assert!(result.is_err(), "release-stage failure must propagate");

        // No git info in the fixture → derive_run_id falls back to "local".
        let summary_path = ctx.config.dist.join("run-local").join("summary.json");
        assert!(
            summary_path.exists(),
            "default summary.json must be written on a pre-publish stage failure"
        );
        let parsed: anodizer_stage_publish::run_summary::RunSummary =
            serde_json::from_str(&fs::read_to_string(&summary_path).expect("read summary"))
                .expect("parse summary");
        assert_eq!(parsed.tag, "v9.9.9-test", "summary must carry the tag");
        assert!(
            parsed.results.is_empty(),
            "publish never ran -> empty publisher table"
        );
        assert_eq!(parsed.publishers_succeeded, 0);
        assert_eq!(parsed.publishers_failed, 0);
        assert!(
            !parsed.irreversibly_published,
            "nothing published -> recovery may roll back safely"
        );
    }

    #[test]
    fn pipeline_emits_summary_when_announce_is_skipped_via_skip_flag() {
        use anodizer_core::context::ContextOptions;
        use anodizer_stage_announce::AnnounceStage;

        let tmp = TempDir::new().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // Build a pipeline whose only stage is AnnounceStage and skip
        // it via `--skip=announce`. The summary still lands on disk
        // because Pipeline::run owns emit_summary and invokes it after
        // the stage loop, regardless of whether the stage ran.
        let mut p = Pipeline::new();
        p.add(Box::new(AnnounceStage));

        let opts = ContextOptions {
            summary_json_path: Some(summary_path.clone()),
            skip_stages: vec!["announce".to_string()],
            ..ContextOptions::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        ctx.publish_report = Some(anodizer_core::publish_report::PublishReport::default());

        let _dist_guard = isolate_dist(&mut ctx);
        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log).expect("pipeline run");

        // The stage was skipped — but the summary must STILL be written.
        // Regression: an earlier shape put emit_summary inside
        // AnnounceStage::run, where a skipped stage never reached it.
        // Pipeline must own emit_summary so operator-skip can't suppress
        // the summary side-effect.
        assert!(
            summary_path.exists(),
            "summary.json must be written even when announce is operator-skipped",
        );
    }

    // -----------------------------------------------------------------------
    // Per-crate lifecycle (before / after) stage placement + no-double-fire
    //
    // `crates[].before` / `crates[].after` must fire per crate in a full
    // release via BeforeCrateStage / AfterCrateStage. Publish-only fires the
    // same hooks via its own per-crate loop (`run_per_crate_lifecycle_hooks`)
    // and must NOT also carry these stages, or a publish-only run would
    // double-fire. These tests pin that invariant structurally.
    // -----------------------------------------------------------------------

    #[test]
    fn release_pipeline_has_before_crate_at_head_after_crate_at_tail() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        let before = idx(&names, "before", "build_release_pipeline");
        let after = idx(&names, "after", "build_release_pipeline");
        let build = idx(&names, "build", "build_release_pipeline");
        let publish = idx(&names, "publish", "build_release_pipeline");
        assert!(
            before < build,
            "before must precede build (pipeline head); got {names:?}"
        );
        assert!(
            after > publish,
            "after must follow publish (pipeline tail); got {names:?}"
        );
        // after is the terminal stage.
        assert_eq!(
            names.last().copied(),
            Some("after"),
            "after must be the final stage; got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_omits_before_after_crate_stages() {
        // Publish-only owns its per-crate before/after firing in
        // `run_per_crate_lifecycle_hooks`; the pipeline must NOT carry the
        // stages too, or per-crate publish-only would fire each hook twice.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        assert!(
            !names.contains(&"before"),
            "build_publish_only_pipeline must not contain a `before` stage \
             (publish-only fires it via its own per-crate loop); got {names:?}"
        );
        assert!(
            !names.contains(&"after"),
            "build_publish_only_pipeline must not contain an `after` stage; got {names:?}"
        );
    }

    #[test]
    fn merge_and_publish_pipelines_omit_before_after_crate_stages() {
        // Neither legacy publish nor merge runs the per-crate Rust loop, and
        // their top-level before/after are handled separately by the
        // dispatcher; they must not carry the per-crate lifecycle stages.
        let publish = build_publish_pipeline();
        let merge = build_merge_pipeline();
        for (name, names) in [
            ("build_publish_pipeline", publish.stage_names()),
            ("build_merge_pipeline", merge.stage_names()),
        ] {
            assert!(
                !names.contains(&"before") && !names.contains(&"after"),
                "{name} must not contain before/after crate stages; got {names:?}"
            );
        }
    }
}
