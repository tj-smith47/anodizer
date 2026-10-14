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

    /// The reproducible timestamp of this run, in seconds since
    /// 1970-01-01 UTC.
    ///
    /// The source date the environment sets (`SOURCE_DATE_EPOCH`, then
    /// `ANODIZE_SOURCE_DATE_EPOCH`, read by
    /// [`crate::sde::ambient_epoch_with_env`]) wins; otherwise the HEAD
    /// committer time (`CommitTimestamp`). Every writer of a build date
    /// reads this one value: the `SOURCE_DATE_EPOCH` a `reproducible: true`
    /// build passes to cargo, the binary mtime it sets, release and source
    /// archive entries, PyPI wheels and sdists, the docker `created` label
    /// and build env, and SBOM timestamps. A binary and the archive holding
    /// it therefore carry one date.
    ///
    /// Returns `Ok(None)` when the environment sets no source date and no
    /// commit is known (writers then leave the default wall-clock stamp),
    /// and the reader's error when a source date variable is malformed.
    pub fn resolve_reproducible_mtime(&self) -> anyhow::Result<Option<u64>> {
        crate::sde::reproducible_epoch_with_env(
            self.env_source(),
            self.template_vars()
                .get("CommitTimestamp")
                .map(String::as_str),
        )
    }

    /// The timestamp the determinism state was seeded with when a stage has
    /// seeded it (the build stage seeds it from
    /// [`Self::resolve_reproducible_mtime`]), otherwise
    /// [`Self::resolve_reproducible_mtime`] itself.
    ///
    /// The docker `created` label, the docker build env and SBOM timestamps
    /// read this, so a run that skips the build stage still stamps them with
    /// the run's one date.
    pub fn reproducible_epoch(&self) -> anyhow::Result<Option<i64>> {
        if let Some(state) = self.determinism.as_ref() {
            return Ok(Some(state.sde));
        }
        Ok(self
            .resolve_reproducible_mtime()?
            .and_then(|secs| i64::try_from(secs).ok()))
    }

    /// Derive the verbosity level from context options.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_flags(self.options.quiet, self.options.verbose, self.options.debug)
    }

    /// Resolve the user's `retry:` block into a concrete `RetryPolicy`,
    /// applying defaults when `retry:` is unset. Equivalent to
    /// `ctx.config.retry.unwrap_or_default().to_policy()` but centralizes
    /// the lookup so a future refactor can hang validation / clamping off
    /// a single place.
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
    /// This is a READ, not a new anchor. The anchor belongs to the enclosing
    /// [`crate::retry::PublisherRetryScope`], installed once per publisher
    /// invocation at the dispatch site, so every read inside one invocation —
    /// an auth exchange, the publish request, a cleanup — observes the SAME
    /// deadline and a wedged remote cannot spend the budget once per read. A
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
        // a secret issued later through an `env_source` mutation still reaches
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

    /// The logger a render of `render` writes through. Emission validation,
    /// the pre-publish guard and the publisher can each render the same
    /// manifest in one run, and which of them runs first differs per
    /// command, so the first render of a key gets `log` and every later one
    /// gets a logger that prints no verbose, warning or status line. An
    /// error still reaches the caller through its `Result`.
    ///
    /// `render` names one rendered document, for example
    /// `homebrew cask 'mytool'`: a key per publisher would silence the
    /// second crate's render in the same pass.
    pub fn claim_render_log(&self, render: &str, log: &StageLogger) -> StageLogger {
        if self
            .render_log_claims
            .borrow_mut()
            .insert(render.to_string())
        {
            log.clone()
        } else {
            log.capped_at(Verbosity::Quiet)
        }
    }
}
