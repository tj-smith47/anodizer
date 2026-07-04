//! Shared asset-upload orchestration for the forge release backends.
//!
//! The GitHub, GitLab, and Gitea backends create their release through
//! forge-specific API calls, but the policy around uploading the release's
//! artifacts is one decision set: prepare the entry list (bailing on missing
//! files), bound the parallelism, pace the upload starts, reconcile each
//! asset against any same-named remote (the immutable-releases invariant in
//! [`crate::classify_asset_conflict`]), and drain the task set. That policy
//! lives once here as [`run_upload_loop`], parameterized over a
//! [`ForgeAssetClient`]; the per-forge modules implement only the API-shaped
//! pieces (probe / delete / upload).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// Pre-upload reconciliation verdict for one would-be asset name.
pub(crate) enum AssetPresence {
    /// This forge reconciles name conflicts reactively inside
    /// [`ForgeAssetClient::upload_asset`] (GitHub's 422 `already_exists`
    /// recovery, which probes size + partial-upload state on conflict); the
    /// driver goes straight to the upload without a local-size stat.
    Reactive,
    /// No same-named remote asset exists.
    Absent,
    /// A same-named remote asset exists; `size` carries its byte count when
    /// the forge can read it (`None` = present but size unreadable).
    Present { size: Option<u64> },
}

/// Terminal disposition of one asset task, threaded back to the drain so
/// the per-artifact verbose line reports what actually happened — a resume
/// run's idempotent skips must not read as uploads.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UploadOutcome {
    /// The artifact's bytes were uploaded.
    Uploaded(String),
    /// A byte-identical remote asset already existed; nothing was sent.
    SkippedIdentical(String),
    /// GitHub only: a stale published asset reappeared after delete+retry
    /// (eventual consistency) and its bytes were kept. The upload path
    /// already warned at default visibility, so the drain stays silent.
    SkippedStaleKept(String),
}

/// The API-shaped operations one forge contributes to the shared upload
/// loop. Implementations own their client handles / coordinates so the
/// returned futures are `'static` and can be moved into spawned tasks.
pub(crate) trait ForgeAssetClient: Send + Sync + 'static {
    /// Forge label used in shared driver messages (`"github"` / `"gitlab"` /
    /// `"gitea"`).
    fn forge(&self) -> &'static str;

    /// Hook run once before the parallel loop spawns, only when at least one
    /// entry will upload (GitHub blocks here until the just-created release
    /// is readable; the reqwest-based forges need nothing).
    fn before_uploads(&self, entry_count: usize) -> impl Future<Output = Result<()>> + Send;

    /// Probe the remote for a same-named asset.
    fn probe_asset(&self, file_name: &str) -> impl Future<Output = Result<AssetPresence>> + Send;

    /// Delete the same-named remote asset (only called after
    /// [`AssetPresence::Present`] classified as an opted-in overwrite).
    fn delete_asset(&self, file_name: &str) -> impl Future<Output = Result<()>> + Send;

    /// Upload one artifact. Retry policy and conflict recovery beyond the
    /// driver's pre-upload probe are the implementation's responsibility.
    /// Returns the [`UploadOutcome`] so a reactive in-upload skip (GitHub's
    /// 422 `already_exists` recovery) reaches the drain's log honestly.
    fn upload_asset(
        &self,
        path: &Path,
        file_name: &str,
    ) -> impl Future<Output = Result<UploadOutcome>> + Send;
}

/// The shared upload-loop policy knobs, resolved once per backend run.
pub(crate) struct UploadPlan {
    pub replace_existing_artifacts: bool,
    pub upload_concurrency: usize,
    pub upload_pace: Duration,
}

impl UploadPlan {
    /// Resolve the loop policy from env + config.
    ///
    /// Upload concurrency is deliberately separate from
    /// `ctx.options.parallelism` (which governs build concurrency): large
    /// artifact lists must not blast 100+ simultaneous uploads, the exact
    /// burst that trips GitHub's secondary rate limit and stresses any
    /// forge. Precedence: `ANODIZER_GITHUB_UPLOAD_CONCURRENCY` env >
    /// `release.upload_concurrency` > 4. The pace (minimum interval between
    /// successive upload STARTS) layers on top of the cap; see
    /// [`resolve_upload_pace`]. The env names keep their historical
    /// `GITHUB` infix — they predate the loop being shared — and apply to
    /// every forge.
    pub(crate) fn resolve<E: anodizer_core::EnvSource + ?Sized>(
        release_cfg: &anodizer_core::config::ReleaseConfig,
        env: &E,
        replace_existing_artifacts: bool,
    ) -> Self {
        let upload_concurrency: usize = env
            .var("ANODIZER_GITHUB_UPLOAD_CONCURRENCY")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&n| n > 0)
            .or_else(|| release_cfg.upload_concurrency.filter(|&n| n > 0))
            .unwrap_or(4) as usize;
        Self {
            replace_existing_artifacts,
            upload_concurrency,
            upload_pace: resolve_upload_pace(release_cfg, env),
        }
    }
}

/// Resolve the proactive upload pace — the minimum interval between successive
/// asset-upload *starts* — applying the env override, then the config value,
/// then the default.
///
/// Precedence (first match wins), mirroring the
/// `ANODIZER_GITHUB_UPLOAD_CONCURRENCY` -> `release.upload_concurrency` chain:
/// 1. `ANODIZER_GITHUB_UPLOAD_PACE_MS` — integer milliseconds. `0` disables
///    pacing (returns `Duration::ZERO`); a non-parsing value is ignored and
///    falls through to the config / default.
/// 2. `release.upload_pace` (a humantime string), via
///    [`anodizer_core::config::ReleaseConfig::resolved_upload_pace`].
/// 3. [`anodizer_core::config::ReleaseConfig::DEFAULT_UPLOAD_PACE`] (200 ms).
///
/// `Duration::ZERO` is the "pacing disabled" sentinel; the loop skips the
/// pace sleep entirely when it is returned. Pure (the env source is injected)
/// so the precedence is unit-testable without mutating the process env.
pub(crate) fn resolve_upload_pace<E: anodizer_core::EnvSource + ?Sized>(
    release_cfg: &anodizer_core::config::ReleaseConfig,
    env: &E,
) -> Duration {
    if let Some(raw) = env.var("ANODIZER_GITHUB_UPLOAD_PACE_MS")
        && let Ok(ms) = raw.trim().parse::<u64>()
    {
        return Duration::from_millis(ms);
    }
    release_cfg.resolved_upload_pace()
}

/// Drive the bounded-parallel upload of `artifact_entries` through `client`.
///
/// 1. Prepare the entry list: resolve each entry's upload name (custom name
///    or file name) and collect missing files — ANY missing file fails the
///    whole loop before a single byte uploads.
/// 2. Run `client.before_uploads` once (when at least one entry uploads).
/// 3. Spawn one task per entry into a `JoinSet`, bounded by a semaphore of
///    `plan.upload_concurrency`, spacing successive spawns by
///    `plan.upload_pace` (±20 % jitter) so concurrent releases don't
///    synchronise their bursts.
/// 4. Per task: probe the remote ([`AssetPresence`]), classify via
///    [`crate::classify_asset_conflict`] — a byte-identical remote asset is
///    an idempotent skip REGARDLESS of `replace_existing_artifacts`, a
///    differing one is deleted first only when the user opted in — then
///    upload.
/// 5. Drain: first task failure aborts the loop; a panicked task surfaces as
///    `release: upload task panicked`.
pub(crate) async fn run_upload_loop<C: ForgeAssetClient>(
    client: Arc<C>,
    plan: &UploadPlan,
    artifact_entries: &[(PathBuf, Option<String>)],
    log: &StageLogger,
) -> Result<()> {
    let mut missing_files = Vec::new();
    let prepared_entries: Vec<(PathBuf, String)> = artifact_entries
        .iter()
        .filter_map(|(path, custom_name)| {
            if !path.exists() {
                missing_files.push(path.display().to_string());
                return None;
            }
            let file_name = if let Some(name) = custom_name {
                name.clone()
            } else {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "artifact".to_string())
            };
            Some((path.clone(), file_name))
        })
        .collect();

    if !missing_files.is_empty() {
        anyhow::bail!(
            "the following artifact files are missing:\n  {}",
            missing_files.join("\n  ")
        );
    }

    if !prepared_entries.is_empty() {
        client.before_uploads(prepared_entries.len()).await?;
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(std::cmp::max(
        plan.upload_concurrency,
        1,
    )));
    let replace_existing_artifacts = plan.replace_existing_artifacts;
    let mut join_set = tokio::task::JoinSet::new();

    for (idx, (path, file_name)) in prepared_entries.into_iter().enumerate() {
        // Space upload STARTS, not completions: the semaphore alone would
        // admit the first `upload_concurrency` POSTs in the same instant.
        // Skipped for the first task (no prior start to space from) and when
        // pacing is disabled.
        if idx > 0 && !plan.upload_pace.is_zero() {
            tokio::time::sleep(anodizer_core::retry::jitter_duration(plan.upload_pace)).await;
        }
        let sem = semaphore.clone();
        let client = client.clone();

        join_set.spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

            // The pre-upload reconciliation runs REGARDLESS of
            // `replace_existing_artifacts`: a byte-identical asset is not an
            // "overwrite" — the user's flag guards against replacing
            // DIFFERENT bytes, not against a no-op — so a re-run /
            // --resume-release never mutates already-published bytes and
            // never trips a duplicate-name conflict.
            match client.probe_asset(&file_name).await? {
                AssetPresence::Reactive => {}
                presence => {
                    let (remote_present, remote_size) = match presence {
                        AssetPresence::Absent => (false, None),
                        AssetPresence::Present { size } => (true, size),
                        AssetPresence::Reactive => unreachable!("handled above"),
                    };
                    let local_size = tokio::fs::metadata(&path)
                        .await
                        .with_context(|| {
                            format!(
                                "{}: stat local artifact '{}' for size comparison",
                                client.forge(),
                                file_name
                            )
                        })?
                        .len();
                    match crate::classify_asset_conflict(
                        replace_existing_artifacts,
                        remote_present,
                        remote_size,
                        local_size,
                    ) {
                        crate::AssetConflict::IdenticalSkip => {
                            // A prior attempt uploaded byte-identical
                            // content: pure no-op.
                            return Ok::<UploadOutcome, anyhow::Error>(
                                UploadOutcome::SkippedIdentical(file_name),
                            );
                        }
                        crate::AssetConflict::ReplaceDiffering => {
                            client.delete_asset(&file_name).await?;
                        }
                        // No pre-upload bail on a forbidden conflict: the
                        // forge API surfaces the duplicate on upload with
                        // its own conflict semantics.
                        crate::AssetConflict::ConflictForbidden
                        | crate::AssetConflict::NoConflict => {}
                    }
                }
            }

            client.upload_asset(&path, &file_name).await
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(UploadOutcome::Uploaded(file_name))) => {
                log.verbose(&format!("uploaded artifact {}", file_name));
            }
            Ok(Ok(UploadOutcome::SkippedIdentical(file_name))) => {
                log.verbose(&format!(
                    "skipped byte-identical asset {} — already uploaded",
                    file_name
                ));
            }
            // The stale-kept arm already reported itself with a
            // default-visibility warn; a second drain line would be noise.
            Ok(Ok(UploadOutcome::SkippedStaleKept(_))) => {}
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(anyhow::anyhow!(
                    "release: upload task panicked: {}",
                    join_err
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::Verbosity;
    use std::sync::Mutex;

    /// Scripted mock forge: records every call and replays a fixed
    /// [`AssetPresence`] per probe.
    struct MockForge {
        presence: fn() -> AssetPresence,
        calls: Mutex<Vec<String>>,
        fail_upload: bool,
        panic_upload: bool,
    }

    impl MockForge {
        fn new(presence: fn() -> AssetPresence) -> Self {
            Self {
                presence,
                calls: Mutex::new(Vec::new()),
                fail_upload: false,
                panic_upload: false,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
    }

    impl ForgeAssetClient for MockForge {
        fn forge(&self) -> &'static str {
            "mock"
        }
        async fn before_uploads(&self, entry_count: usize) -> Result<()> {
            self.record(format!("before:{entry_count}"));
            Ok(())
        }
        async fn probe_asset(&self, file_name: &str) -> Result<AssetPresence> {
            self.record(format!("probe:{file_name}"));
            Ok((self.presence)())
        }
        async fn delete_asset(&self, file_name: &str) -> Result<()> {
            self.record(format!("delete:{file_name}"));
            Ok(())
        }
        async fn upload_asset(&self, _path: &Path, file_name: &str) -> Result<UploadOutcome> {
            if self.panic_upload {
                panic!("boom");
            }
            self.record(format!("upload:{file_name}"));
            if self.fail_upload {
                anyhow::bail!("mock upload failed");
            }
            Ok(UploadOutcome::Uploaded(file_name.to_string()))
        }
    }

    fn test_log() -> StageLogger {
        StageLogger::new("release", Verbosity::Normal)
    }

    fn plan(replace: bool) -> UploadPlan {
        UploadPlan {
            replace_existing_artifacts: replace,
            upload_concurrency: 2,
            upload_pace: Duration::ZERO,
        }
    }

    fn write_artifact(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write artifact");
        p
    }

    #[tokio::test]
    async fn missing_file_bails_before_any_forge_call() {
        let client = Arc::new(MockForge::new(|| AssetPresence::Absent));
        let entries = vec![(PathBuf::from("/nonexistent/definitely-missing.bin"), None)];
        let err = run_upload_loop(client.clone(), &plan(false), &entries, &test_log())
            .await
            .expect_err("missing file must bail");
        assert!(
            err.to_string()
                .contains("the following artifact files are missing:"),
            "verbatim missing-files bail: {err}"
        );
        assert!(
            client.calls().is_empty(),
            "no probe/upload may run when any file is missing"
        );
    }

    #[tokio::test]
    async fn reactive_presence_uploads_without_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let client = Arc::new(MockForge::new(|| AssetPresence::Reactive));
        let entries = vec![(a, Some("a.bin".to_string()))];
        run_upload_loop(client.clone(), &plan(false), &entries, &test_log())
            .await
            .expect("upload succeeds");
        assert_eq!(
            client.calls(),
            vec!["before:1", "probe:a.bin", "upload:a.bin"]
        );
    }

    #[tokio::test]
    async fn absent_remote_uploads_without_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let client = Arc::new(MockForge::new(|| AssetPresence::Absent));
        let entries = vec![(a, Some("a.bin".to_string()))];
        run_upload_loop(client.clone(), &plan(true), &entries, &test_log())
            .await
            .expect("upload succeeds");
        assert_eq!(
            client.calls(),
            vec!["before:1", "probe:a.bin", "upload:a.bin"]
        );
    }

    #[tokio::test]
    async fn same_size_remote_skips_upload_regardless_of_replace_flag() {
        for replace in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let a = write_artifact(dir.path(), "a.bin", b"AAAA");
            let client = Arc::new(MockForge::new(|| AssetPresence::Present { size: Some(4) }));
            let entries = vec![(a, Some("a.bin".to_string()))];
            let (log, cap) = StageLogger::with_capture("release", Verbosity::Normal);
            run_upload_loop(client.clone(), &plan(replace), &entries, &log)
                .await
                .expect("idempotent skip succeeds");
            assert_eq!(
                client.calls(),
                vec!["before:1", "probe:a.bin"],
                "byte-identical remote must skip (replace={replace})"
            );
            // The drain must report the skip, not claim an upload.
            let messages = cap.all_messages();
            assert!(
                messages.iter().any(|(level, m)| {
                    *level == anodizer_core::log::LogLevel::Verbose
                        && m == "skipped byte-identical asset a.bin — already uploaded"
                }),
                "skip line missing (replace={replace}): {messages:?}"
            );
            assert!(
                !messages
                    .iter()
                    .any(|(_, m)| m.contains("uploaded artifact")),
                "an idempotent skip must not log an upload (replace={replace}): {messages:?}"
            );
        }
    }

    #[tokio::test]
    async fn uploaded_artifact_logged_for_real_uploads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let client = Arc::new(MockForge::new(|| AssetPresence::Absent));
        let entries = vec![(a, Some("a.bin".to_string()))];
        let (log, cap) = StageLogger::with_capture("release", Verbosity::Normal);
        run_upload_loop(client.clone(), &plan(false), &entries, &log)
            .await
            .expect("upload succeeds");
        assert!(
            cap.all_messages().iter().any(|(level, m)| {
                *level == anodizer_core::log::LogLevel::Verbose && m == "uploaded artifact a.bin"
            }),
            "real upload must keep the historical drain line: {:?}",
            cap.all_messages()
        );
    }

    #[tokio::test]
    async fn differing_remote_with_replace_deletes_then_uploads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let client = Arc::new(MockForge::new(|| AssetPresence::Present { size: Some(9) }));
        let entries = vec![(a, Some("a.bin".to_string()))];
        run_upload_loop(client.clone(), &plan(true), &entries, &test_log())
            .await
            .expect("replace succeeds");
        assert_eq!(
            client.calls(),
            vec!["before:1", "probe:a.bin", "delete:a.bin", "upload:a.bin"]
        );
    }

    #[tokio::test]
    async fn differing_remote_without_replace_uploads_without_delete() {
        // The forge API surfaces the conflict itself; the driver must not
        // pre-delete without the user's opt-in.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let client = Arc::new(MockForge::new(|| AssetPresence::Present { size: Some(9) }));
        let entries = vec![(a, Some("a.bin".to_string()))];
        run_upload_loop(client.clone(), &plan(false), &entries, &test_log())
            .await
            .expect("upload proceeds");
        assert_eq!(
            client.calls(),
            vec!["before:1", "probe:a.bin", "upload:a.bin"]
        );
    }

    #[tokio::test]
    async fn upload_failure_propagates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let mut mock = MockForge::new(|| AssetPresence::Absent);
        mock.fail_upload = true;
        let client = Arc::new(mock);
        let entries = vec![(a, Some("a.bin".to_string()))];
        let err = run_upload_loop(client, &plan(false), &entries, &test_log())
            .await
            .expect_err("upload failure must propagate");
        assert!(err.to_string().contains("mock upload failed"), "{err}");
    }

    #[tokio::test]
    async fn panicked_task_surfaces_as_upload_task_panicked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "a.bin", b"AAAA");
        let mut mock = MockForge::new(|| AssetPresence::Reactive);
        mock.panic_upload = true;
        let client = Arc::new(mock);
        let entries = vec![(a, Some("a.bin".to_string()))];
        let err = run_upload_loop(client, &plan(false), &entries, &test_log())
            .await
            .expect_err("panicked task must surface");
        assert!(
            err.to_string().contains("release: upload task panicked"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn custom_name_falls_back_to_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_artifact(dir.path(), "plain.bin", b"AAAA");
        let b = write_artifact(dir.path(), "renamed-src.bin", b"BBBB");
        let client = Arc::new(MockForge::new(|| AssetPresence::Absent));
        let entries = vec![(a, None), (b, Some("custom.bin".to_string()))];
        run_upload_loop(client.clone(), &plan(false), &entries, &test_log())
            .await
            .expect("uploads succeed");
        let calls = client.calls();
        assert!(calls.contains(&"upload:plain.bin".to_string()), "{calls:?}");
        assert!(
            calls.contains(&"upload:custom.bin".to_string()),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn empty_entry_list_skips_before_uploads_hook() {
        let client = Arc::new(MockForge::new(|| AssetPresence::Absent));
        run_upload_loop(client.clone(), &plan(false), &[], &test_log())
            .await
            .expect("empty list is a no-op");
        assert!(
            client.calls().is_empty(),
            "no before_uploads / probe on an empty upload set"
        );
    }

    mod upload_pace_tests {
        use super::super::resolve_upload_pace;
        use anodizer_core::MapEnvSource;
        use anodizer_core::config::ReleaseConfig;
        use std::time::Duration;

        fn cfg_with_pace(s: &str) -> ReleaseConfig {
            serde_yaml_ng::from_str(&format!("upload_pace: \"{s}\"")).expect("parse release cfg")
        }

        #[test]
        fn defaults_to_200ms_when_unset() {
            let cfg = ReleaseConfig::default();
            let env = MapEnvSource::new();
            assert_eq!(
                resolve_upload_pace(&cfg, &env),
                ReleaseConfig::DEFAULT_UPLOAD_PACE,
            );
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::from_millis(200));
        }

        #[test]
        fn config_value_overrides_default() {
            let cfg = cfg_with_pace("1s");
            let env = MapEnvSource::new();
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::from_secs(1));
        }

        #[test]
        fn config_zero_disables_pacing() {
            // "0s" must resolve to the ZERO sentinel so the loop skips pacing.
            let cfg = cfg_with_pace("0s");
            let env = MapEnvSource::new();
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::ZERO);
        }

        #[test]
        fn env_override_takes_precedence_over_config() {
            let cfg = cfg_with_pace("1s");
            let env = MapEnvSource::new().with("ANODIZER_GITHUB_UPLOAD_PACE_MS", "50");
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::from_millis(50));
        }

        #[test]
        fn env_zero_disables_pacing_even_with_config_set() {
            let cfg = cfg_with_pace("1s");
            let env = MapEnvSource::new().with("ANODIZER_GITHUB_UPLOAD_PACE_MS", "0");
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::ZERO);
        }

        #[test]
        fn garbage_env_falls_through_to_config() {
            let cfg = cfg_with_pace("1s");
            let env = MapEnvSource::new().with("ANODIZER_GITHUB_UPLOAD_PACE_MS", "not-a-number");
            assert_eq!(resolve_upload_pace(&cfg, &env), Duration::from_secs(1));
        }
    }

    mod upload_plan_tests {
        use super::super::UploadPlan;
        use anodizer_core::MapEnvSource;
        use anodizer_core::config::ReleaseConfig;

        #[test]
        fn concurrency_defaults_to_four() {
            let plan = UploadPlan::resolve(&ReleaseConfig::default(), &MapEnvSource::new(), false);
            assert_eq!(plan.upload_concurrency, 4);
            assert!(!plan.replace_existing_artifacts);
        }

        #[test]
        fn config_concurrency_overrides_default() {
            let cfg: ReleaseConfig =
                serde_yaml_ng::from_str("upload_concurrency: 2").expect("parse");
            let plan = UploadPlan::resolve(&cfg, &MapEnvSource::new(), true);
            assert_eq!(plan.upload_concurrency, 2);
            assert!(plan.replace_existing_artifacts);
        }

        #[test]
        fn env_concurrency_overrides_config() {
            let cfg: ReleaseConfig =
                serde_yaml_ng::from_str("upload_concurrency: 2").expect("parse");
            let env = MapEnvSource::new().with("ANODIZER_GITHUB_UPLOAD_CONCURRENCY", "8");
            assert_eq!(UploadPlan::resolve(&cfg, &env, false).upload_concurrency, 8);
        }

        #[test]
        fn zero_and_garbage_env_fall_through() {
            let cfg: ReleaseConfig =
                serde_yaml_ng::from_str("upload_concurrency: 2").expect("parse");
            for bad in ["0", "nope"] {
                let env = MapEnvSource::new().with("ANODIZER_GITHUB_UPLOAD_CONCURRENCY", bad);
                assert_eq!(
                    UploadPlan::resolve(&cfg, &env, false).upload_concurrency,
                    2,
                    "env '{bad}' must fall through to config"
                );
            }
        }
    }
}
