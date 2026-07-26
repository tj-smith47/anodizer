use super::*;

impl Context {
    /// Return the current `Version` template variable, or an empty string if
    /// not yet populated.
    pub fn version(&self) -> String {
        self.template_vars
            .get("Version")
            .cloned()
            .unwrap_or_default()
    }

    /// Reproducible-mtime seed shared by every stage that stamps a build
    /// timestamp into a produced artifact (release archives, source archives,
    /// PyPI wheels + sdists).
    ///
    /// Resolution ladder, single-sourced here so archives and wheels never
    /// pick different timestamps in one run:
    ///
    /// 1. when ANY build in the crate universe is `reproducible: true`, the
    ///    commit timestamp wins outright — a reproducible build pins its own
    ///    output to the commit, so a stray ambient `SOURCE_DATE_EPOCH` must
    ///    not override it;
    /// 2. otherwise `SOURCE_DATE_EPOCH` (the standard reproducibility
    ///    contract, set by the determinism harness on every child), falling
    ///    back to the commit timestamp.
    ///
    /// Returns `None` when neither a commit timestamp nor `SOURCE_DATE_EPOCH`
    /// is available (writers then leave the default wall-clock stamp).
    pub fn resolve_reproducible_mtime(&self) -> Option<u64> {
        let any_reproducible = self.config.crate_universe().into_iter().any(|c| {
            c.builds
                .as_ref()
                .is_some_and(|builds| builds.iter().any(|b| b.reproducible.unwrap_or(false)))
        });
        let commit_ts = self
            .template_vars()
            .get("CommitTimestamp")
            .and_then(|ts| ts.parse::<u64>().ok());
        if any_reproducible {
            commit_ts
        } else {
            self.env_var("SOURCE_DATE_EPOCH")
                .and_then(|s| s.parse::<u64>().ok())
                .or(commit_ts)
        }
    }

    /// Derive the verbosity level from context options.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_flags(self.options.quiet, self.options.verbose, self.options.debug)
    }

    /// Resolve the user's `retry:` block into a concrete `RetryPolicy`,
    /// applying defaults when `retry:` is unset. Equivalent to
    /// `ctx.config.retry.unwrap_or_default().to_policy()` but centralizes
    /// the lookup so a future refactor can hang validation / clamping off
    /// a single seam.
    pub fn retry_policy(&self) -> crate::retry::RetryPolicy {
        self.config.retry.unwrap_or_default().to_policy()
    }

    /// The absolute wall-clock deadline the current publisher invocation's
    /// retry ladders must not sleep past: the invocation's anchor plus
    /// `retry.max_elapsed` (or [`crate::retry::DEFAULT_MAX_ELAPSED`], 15 min,
    /// when unset) — so a publisher that threads this into
    /// [`crate::retry::retry_sync_deadline`] /
    /// [`crate::retry::retry_async_deadline`] is bounded by default and the
    /// operator can raise or lower the ceiling with one config field.
    ///
    /// This is a READ, not a mint. The anchor belongs to the enclosing
    /// [`crate::retry::PublisherRetryScope`], installed once per publisher
    /// invocation at the dispatch seam, so every seam inside one invocation —
    /// an auth exchange, the publish request, a cleanup — observes the SAME
    /// deadline and a wedged remote cannot spend the budget once per seam. A
    /// caller outside any such scope (a stage-level retry ladder) anchors at
    /// the moment of the call.
    ///
    /// Always `Some`, except when the configured budget is so large that the
    /// deadline is not representable — an unrepresentable ceiling is no
    /// ceiling, and `None` is how the engines spell unbounded. That `None` is
    /// also what a caller with no [`Context`] at all passes them.
    pub fn retry_deadline(&self) -> Option<std::time::Instant> {
        let budget = self
            .config
            .retry
            .unwrap_or_default()
            .max_elapsed_duration()
            .unwrap_or(crate::retry::DEFAULT_MAX_ELAPSED);
        crate::retry::current_budget_anchor()
            .unwrap_or_else(std::time::Instant::now)
            .checked_add(budget)
    }

    /// Create a [`StageLogger`] for the given stage name, pre-attached to
    /// the context's env-pairs list so that subprocess stderr / stdout
    /// flowing through [`StageLogger::check_output`] is automatically
    /// redacted. The env list combines the template-engine env
    /// (process + config + `.env` files) and the current `std::env::vars`
    /// snapshot, so any secret value reachable to a hook or subprocess is
    /// available for scrubbing.
    pub fn logger(&self, stage: &'static str) -> StageLogger {
        // Snapshot the current redaction env into the shared cell at
        // construction so a secret injected via `template_vars_mut().set_env`
        // (which has no mutation hook) is captured, matching the historical
        // snapshot-at-`logger()` behavior. The cell stays shared afterward, so
        // a secret minted later through an `env_source` mutation still reaches
        // this logger via `refresh_secret_env` at that mutation point.
        self.refresh_secret_env();
        #[allow(unused_mut)]
        let mut log =
            StageLogger::new(stage, self.verbosity()).with_shared_env(Arc::clone(&self.secret_env));
        #[cfg(feature = "test-helpers")]
        if let Some(cap) = &self.log_capture {
            log = log.with_capture_handle(cap.clone());
        }
        log
    }
}
