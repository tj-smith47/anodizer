//! Pipeline-stage classification shared across entry points.
//!
//! The single definition of which release-pipeline stages reach upstream,
//! consumed by both hermetic modes: `release --prepare` (skips every
//! upstream-touching stage) and the determinism harness's side-effect skip
//! set (which layers host-state-only extras on top). Two hand-maintained
//! copies of this set drifted once — `--prepare` kept building and pushing
//! docker images while documenting a publish-nothing contract.

/// Stage names that reach upstream when they run: uploads, registry
/// pushes, live API calls, announcements, post-publish verification.
///
/// Order mirrors the stage positions in the release pipeline so reviewers
/// scanning this list against the builder can pattern-match. Listed
/// exhaustively (no `starts_with` / glob matching) so a new stage with a
/// similar name (e.g. `docker-extra`) doesn't accidentally inherit the
/// classification. Adding a future upstream-touching stage to the release
/// pipeline MUST add its stage name here — both `--prepare` and the
/// determinism harness derive their skip sets from this constant.
pub const UPSTREAM_STAGES: &[&str] = &[
    // GitHub release creation + asset upload.
    "release",
    // Docker image build+push and cosign signature push. The push flag is
    // disabled only for snapshot/dry-run, so a real-tag hermetic run that
    // fails to skip these publishes images upstream.
    "docker",
    "docker-sign",
    // Object-storage uploads.
    "blob",
    // Package-manager publishers (cargo, chocolatey, winget, …) and the
    // snapcraft store upload.
    "publish",
    "snapcraft-publish",
    // Notifications, then live GitHub API verification of the published
    // release.
    "announce",
    "verify-release",
];
