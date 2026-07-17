use super::*;

impl Context {
    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub fn is_snapshot(&self) -> bool {
        self.options.snapshot
    }

    /// Whether this run builds only a subset of the configured targets — either
    /// a `--split` / `--targets` determinism shard (`partial_target`) or a
    /// host-only `--single-target` build.
    ///
    /// A publisher whose eligible artifact is legitimately absent on a
    /// restricted build (e.g. a Windows-only publisher on a Linux single-target
    /// snapshot) must self-skip its schema validation rather than error: the
    /// artifact lands on another target, not a misconfiguration. On a FULL build
    /// the same absence IS a misconfiguration and must surface. `--single-target`
    /// (`single_target`) is clap-exclusive with `--targets` / `--host-targets`
    /// (which populate `partial_target`), but NOT with `--split` (a split shard
    /// resolves its own `partial_target` from `partial.by` yet may still be
    /// scoped to the host target), so both signals can be set at once; this OR
    /// is the single "restricted build" predicate the per-publisher validators
    /// gate their no-artifact skip on, correct whether one or both are set.
    pub fn is_target_restricted_build(&self) -> bool {
        self.options.partial_target.is_some() || self.options.single_target.is_some()
    }

    /// Whether this run is `anodizer release --publish-only` (publishing a
    /// preserved dist rather than building from source).
    ///
    /// Build-time concerns (notably the `binary_signs:` per-binary signing
    /// loop, whose output is embedded into archives at build time and has no
    /// publish-time consumer) are gated off this in publish-only mode, where
    /// the runner carries only publish-time credentials.
    pub fn is_publish_only(&self) -> bool {
        self.options.publish_only
    }

    pub fn is_strict(&self) -> bool {
        self.options.strict
    }

    /// Effective preflight strictness: the global `--strict`, the scoped
    /// `--strict-preflight`, or the config-level `preflight.strict` — any one
    /// turns it on. Under strict preflight, indeterminate probe outcomes
    /// (Unknown publisher state, 5xx / rate-limit / network failure /
    /// undeterminable permissions) become hard blockers instead of warnings.
    /// Definitive failures keep their required→blocker / optional→warning
    /// severity either way.
    pub fn preflight_is_strict(&self) -> bool {
        self.options.strict || self.options.strict_preflight || self.config.preflight.strict
    }

    /// Toggle the runtime strict-render flag (see the `render_strict` field).
    ///
    /// The pre-publish guard calls this with `true` before its render pass and
    /// restores the prior value after, so render-error swallowing is suppressed
    /// only for that in-memory validation — production publish renders stay
    /// lenient unless the user passed the global `--strict`. Returns the prior
    /// value so the caller can restore it.
    pub fn set_render_strict(&self, on: bool) -> bool {
        self.render_strict.replace(on)
    }

    /// Whether template renders should propagate errors (strict) rather than
    /// warn-and-fall-back-to-raw (lenient).
    ///
    /// True when EITHER the guard's transient `render_strict` flag is set OR the
    /// user passed the global `--strict`, so a malformed publisher/announce
    /// template fails loud under the guard and under `--strict` everywhere.
    pub fn render_is_strict(&self) -> bool {
        self.render_strict.get() || self.is_strict()
    }

    /// In strict mode, return an error. In normal mode, log a warning and continue.
    /// Use this for any situation where a configured feature silently skips.
    pub fn strict_guard(&self, log: &crate::log::StageLogger, msg: &str) -> anyhow::Result<()> {
        if self.options.strict {
            anyhow::bail!("{} (strict mode)", msg);
        }
        log.warn(msg);
        Ok(())
    }

    /// Defense-in-depth helper for upload-style stages.
    ///
    /// Returns `true` (after logging the skip) when the context is in snapshot
    /// mode. Stages that perform external uploads (registries, package indexes,
    /// object storage, snap store, …) call this at entry so they no-op even
    /// when invoked directly without the orchestration layer's auto-skip.
    /// Centralising the check keeps every publish stage consistent and avoids
    /// per-stage copy-paste.
    pub fn skip_in_snapshot(&self, log: &crate::log::StageLogger, stage: &str) -> bool {
        if self.is_snapshot() {
            // The stage name stays in the line: this guard fires on direct
            // stage invocation, where no pipeline section header has named
            // the stage yet.
            log.status(&format!("skipped {stage} — snapshot mode"));
            true
        } else {
            false
        }
    }

    pub fn is_nightly(&self) -> bool {
        self.options.nightly
    }

    /// Set the `ReleaseURL` template variable.
    ///
    /// Should be called after a GitHub release is created, with the URL of
    /// the created release (e.g. `https://github.com/owner/repo/releases/tag/v1.0.0`).
    pub fn set_release_url(&mut self, url: &str) {
        self.template_vars.set("ReleaseURL", url);
    }
}
