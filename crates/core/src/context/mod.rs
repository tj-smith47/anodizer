use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::env_source::{EnvSource, ProcessEnvSource};
use crate::git::GitInfo;
use crate::log::{StageLogger, Verbosity};
use crate::partial::PartialTarget;
use crate::publish_report::PublishReport;
use crate::publisher_kind::PublisherKind;
use crate::scm::ScmTokenType;
use crate::template::TemplateVars;
use crate::verify_release_summary::VerifyReleaseSummary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use strum::IntoEnumIterator;

mod mode;
mod options;
mod populate;
mod render;
mod runtime;
mod skip;
mod state;
#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;

pub use options::*;
pub use populate::{map_arch_to_goarch, map_os_to_goos};
pub use skip::*;
pub use state::*;

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    /// Stage→stage handoff outputs (changelog text, header/footer, etc.).
    pub stage_outputs: StageOutputs,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
    /// The resolved SCM token type (GitHub, GitLab, or Gitea).
    pub token_type: ScmTokenType,
    /// Aggregated skips from per-sub-config loops (signs, docker_signs,
    /// publishers, …). Drained by the pipeline runner at end-of-pipeline so
    /// the summary shows what was intentionally skipped — mirroring
    /// the skip-memento pattern. The inner `Arc<Mutex<…>>`
    /// lets parallel stage workers contribute without extra plumbing.
    pub skip_memento: crate::pipe_skip::SkipMemento,
    /// Per-expectation skips recorded by the emission-validate pass on a
    /// target-restricted build (an expectation whose target subset was not
    /// built in this run, or a cross-platform aggregate with no eligible
    /// artifact). Kept SEPARATE from [`Self::skip_memento`] on purpose: that
    /// memento is drained into the default-visible end-of-pipeline summary,
    /// while these skips surface only as verbose lines plus an aggregate
    /// count in the stage's one RESULT line — a sharded run would otherwise
    /// print one summary line per unbuilt-target expectation.
    pub emission_skips: crate::pipe_skip::SkipMemento,
    /// Trait-based publisher dispatch report, set by `PublishStage::run`
    /// when the per-publisher dispatcher finishes. `None` until the
    /// publish stage executes (or when publishing is skipped entirely
    /// via snapshot mode / `--skip=publish`). Downstream stages
    /// (SnapcraftPublishStage, AnnounceStage, future Submitter-group
    /// stages) consult this to apply the submitter-gate / announce-gate
    /// rules — see `PublishReport::any_failed`.
    pub publish_report: Option<PublishReport>,
    /// Whether `PublishStage::run` entered its body this run. Set before
    /// the pre-dispatch guards (rerun refusal, runtime allowlist), so a
    /// guard abort leaves this `true` with `publish_report` still `None`
    /// — the summary placeholder row uses the pair to distinguish
    /// "publish skipped" from "publish aborted before dispatch".
    pub publish_attempted: bool,
    /// Verify-release verdict, set by `VerifyReleaseStage::run` immediately
    /// before it returns (clean pass OR `bail!`). `None` until the gate runs
    /// its checks — it stays `None` on the disabled / skipped / dry-run /
    /// snapshot early-returns, where no published release exists to verify.
    ///
    /// Read by the run-summary builder so the end-of-pipeline Summary states
    /// the verify-release outcome on a SEPARATE axis from the publisher rows:
    /// the gate runs after the irreversible publish, so the publishes still
    /// read `succeeded` while this slot records whether the published release
    /// has unverified defects to investigate.
    pub verify_release: Option<VerifyReleaseSummary>,
    /// Pre-submitter verify-release gate, installed once by the CLI's
    /// pipeline-composition layer right after construction (never by a
    /// stage). Invoked by `stage-publish`'s dispatcher immediately before
    /// the first Submitter-group (one-way-door) publisher would run:
    /// `Ok(true)` clears the gate, `Ok(false)` or `Err` blocks every
    /// Submitter-group publisher for the run with
    /// `SkipReason::VerifyGateBlocked`.
    ///
    /// A plain closure field, not a stage, because `stage-publish` cannot
    /// depend on `stage-verify-release` (the dependency runs the other way:
    /// `stage-verify-release` already depends on `stage-publish` for its
    /// terminal landing checks) — routing the call through `Context`, which
    /// every stage crate depends on, avoids the cycle without inventing a
    /// second verify taxonomy. `Arc` (not a bare `Box`) so the field can be
    /// cheaply cloned out of `&mut Context` before being invoked with
    /// `&mut Context`.
    pub verify_gate: Option<VerifyGate>,
    /// SOURCE_DATE_EPOCH seed + non-determinism allow-list state for the
    /// run. `None` until a stage (typically `BuildStage`) seeds it from
    /// `resolve_reproducible_epoch(commit_timestamp)`; downstream stages
    /// (`stage-sbom`, `stage-archive`, `stage-sign`) read `sde` to derive
    /// deterministic timestamps. Lazy-init by design: tests and snapshot
    /// runs without a clean commit can still proceed.
    pub determinism: Option<crate::DeterminismState>,
    /// Per-publisher outcome override published by `Publisher::run` when
    /// the artifact reached a non-`Succeeded` terminal state but `run`
    /// still returned `Ok` (e.g. chocolatey moderation skip,
    /// winget/krew/homebrew PR-already-exists skip). Dispatch consumes
    /// this slot via `take_pending_outcome()` immediately after `run`
    /// returns Ok so the per-publisher row in the summary table reads
    /// `pending-moderation` / `pending-validation` instead of
    /// `succeeded`. The slot is single-shot: any unread value is
    /// cleared at the start of every `run` call.
    pub pending_outcome: Option<crate::PublisherOutcome>,
    /// Partial [`PublishEvidence`] published by `Publisher::run` BEFORE it
    /// returned `Err`, so a publisher that did irreversible work for the
    /// first N items and then failed on item N+1 can still hand the
    /// rollback path the authoritative record of what actually went live.
    ///
    /// The cargo publisher is the motivating case: a multi-crate publish
    /// that succeeds on crate A then fails on crate B must yank A. On the
    /// `Ok` path `run` returns its evidence directly; on the `Err` path
    /// dispatch consumes this slot via [`Context::take_pending_evidence`]
    /// and records it on the failed publisher's report row so rollback has
    /// something to act on. Single-shot — drained at the start of every
    /// `run` and cleared on the `Ok` path.
    pub pending_evidence: Option<crate::PublishEvidence>,
    /// Distinct set of crate names the build stage actually built — i.e.
    /// those that had at least one in-scope build (or `copy_from`) job after
    /// target resolution. `None` until [`BuildStage`] runs (e.g. merge mode,
    /// which pre-loads artifacts and never invokes the build stage).
    ///
    /// Read by the binary-artifact guard to distinguish "configured a
    /// binary-requiring surface but legitimately had no in-scope target in
    /// this shard" (skip) from "was built yet produced no binary" (a real
    /// mis-scope to fail on). Populated via [`Context::set_built_crate_names`]
    /// and read via [`Context::built_crate_names`].
    built_crate_names: Option<std::collections::HashSet<String>>,
    /// Injectable environment-variable source. Defaults to
    /// [`ProcessEnvSource`] (reads `std::env::var`). Tests inject a
    /// [`MapEnvSource`](crate::MapEnvSource) via
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env)
    /// so deterministic branches can be exercised without mutating the
    /// process env. Read through [`Context::env_var`]; replace via
    /// [`Context::set_env_source`].
    env_source: Arc<dyn EnvSource>,
    /// Live handle to the secret-redaction table shared with every
    /// [`StageLogger`] this context has ever produced via [`Context::logger`]
    /// (each gets a clone of the same `Arc<Mutex<_>>` cell). Refreshed from
    /// [`Context::env_for_redact`] by [`Context::refresh_secret_env`] at every
    /// `env_source` mutation point (`set_env_source`, `set_env_source_arc`,
    /// `begin_cargo_trusted_publishing`, `end_cargo_trusted_publishing`), so a
    /// logger constructed BEFORE a mid-run credential mint — e.g. crates.io
    /// Trusted Publishing minting `CARGO_REGISTRY_TOKEN` into `env_source`
    /// partway through `publish_to_cargo` — still redacts it: `StageLogger::
    /// redact` reads this cell live rather than a frozen construction-time
    /// snapshot.
    secret_env: crate::log::RedactionEnv,
    /// Live crates.io Trusted-Publishing overlay state, set for the duration
    /// of a cargo publish that minted a short-lived token via OIDC. Holds the
    /// minted token (the revoke + yank-injection credential) and the base env
    /// source captured before the overlay was installed (restored on
    /// teardown). `None` on the ambient `auth: token` path and outside a
    /// mint. Managed exclusively through
    /// [`Context::begin_cargo_trusted_publishing`] /
    /// [`Context::end_cargo_trusted_publishing`].
    cargo_trusted_publishing: Option<CargoTrustedPublishing>,
    /// Optional in-memory log-capture handle. When `Some`, every logger
    /// produced by [`Context::logger`] attaches it so the test can read
    /// back aggregated counts of `status` / `warn` / etc. calls without
    /// having to intercept stderr.
    ///
    /// Gated behind the `test-helpers` Cargo feature — production
    /// binaries do not carry the field at all.
    #[cfg(feature = "test-helpers")]
    pub log_capture: Option<crate::log::LogCapture>,
    /// Runtime-togglable strict-render flag, distinct from the user's global
    /// `--strict` (`options.strict`). The pre-publish guard flips this on for
    /// the duration of its in-memory render pass (via [`Context::set_render_strict`])
    /// so EVERY publisher/announce template it renders propagates its error
    /// instead of falling back to the raw string — turning a swallowed
    /// broken-template warning into a release-blocking abort BEFORE any
    /// irreversible publisher fires. Production publish leaves it `false`, so
    /// dry-run / snapshot / nightly stay lenient (warn + raw fallback).
    ///
    /// A `Cell` (not a plain `bool`) because the render path holds only a
    /// shared `&Context`: the guard sets it through its `&mut Context`, then
    /// the deep render helpers toggle-read it through `&Context`.
    /// [`Context::render_is_strict`] ORs this with `is_strict()`, so the user's
    /// global `--strict` also makes every render strict everywhere.
    render_strict: std::cell::Cell<bool>,
    /// When true, announce message BODIES are treated as already-final text and
    /// are NOT run through Tera at send time. Set by `anodizer notify` so an
    /// operator-supplied (possibly untrusted) message — e.g. an on_error error
    /// string — cannot expand an `Env`-reference into a secret when the
    /// provider sends it. Only message bodies are affected; titles and other
    /// templated fields still render normally.
    pub literal_message: bool,
    /// When true (the default), outbound announce message BODIES have
    /// known-secret env values masked before send (same policy as log
    /// redaction). `anodizer notify --allow-secrets` sets this false to send a
    /// secret deliberately over a trusted channel. Only the outbound body is
    /// affected; anodizer's own logs are redacted unconditionally regardless of
    /// this flag.
    pub redact_body: bool,
}

/// Live crates.io Trusted-Publishing overlay state (see the `Context`
/// `cargo_trusted_publishing` field). The `base` is the env source captured
/// before the token overlay was installed, restored verbatim on teardown.
struct CargoTrustedPublishing {
    token: String,
    base: Arc<dyn EnvSource>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        let ctx = Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            stage_outputs: StageOutputs::default(),
            template_vars: vars,
            git_info: None,
            token_type: ScmTokenType::GitHub,
            skip_memento: crate::pipe_skip::SkipMemento::new(),
            emission_skips: crate::pipe_skip::SkipMemento::new(),
            publish_report: None,
            publish_attempted: false,
            verify_release: None,
            verify_gate: None,
            determinism: None,
            pending_outcome: None,
            pending_evidence: None,
            built_crate_names: None,
            env_source: Arc::new(ProcessEnvSource),
            secret_env: Arc::new(Mutex::new(Vec::new())),
            cargo_trusted_publishing: None,
            #[cfg(feature = "test-helpers")]
            log_capture: None,
            render_strict: std::cell::Cell::new(false),
            literal_message: false,
            redact_body: true,
        };
        ctx.refresh_secret_env();
        ctx
    }

    /// Redact known-secret env values from outbound announce text, using the
    /// same combined env (template engine env + process env) and the same
    /// policy as log redaction. Always redacts; gating on `redact_body` is the
    /// caller's responsibility (see `render_message_with_default`).
    pub fn redact(&self, s: &str) -> String {
        crate::redact::with_env(s, &self.env_for_redact())
    }

    /// Read an environment variable through the injected source.
    ///
    /// Production reads `std::env::var(name).ok()`. Tests inject a
    /// [`MapEnvSource`](crate::MapEnvSource) via
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env)
    /// so deterministic branches can be exercised without mutating the
    /// process env.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.env_source.var(name)
    }

    /// Replace the injected environment-variable source.
    ///
    /// Production migration code uses this when wrapping an
    /// already-constructed context; tests reach this indirectly through
    /// [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env).
    pub fn set_env_source<S: EnvSource + 'static>(&mut self, src: S) {
        self.env_source = Arc::new(src);
        self.refresh_secret_env();
    }

    /// Replace the injected environment-variable source with an already-boxed
    /// `Arc<dyn EnvSource>`. Used to RESTORE a previously captured base source
    /// after a temporary overlay (see
    /// [`Context::begin_cargo_trusted_publishing`]) without re-wrapping it.
    pub fn set_env_source_arc(&mut self, src: Arc<dyn EnvSource>) {
        self.env_source = src;
        self.refresh_secret_env();
    }

    /// Overlay a minted crates.io Trusted-Publishing token as
    /// `CARGO_REGISTRY_TOKEN` for the cargo publish+rollback lifecycle.
    ///
    /// The current env source is captured as the base, then wrapped in a
    /// [`LayeredEnvSource`](crate::LayeredEnvSource) that overrides
    /// `CARGO_REGISTRY_TOKEN` with `token`. This makes the token visible to
    /// env-driven paths that read through [`Context::env_source`] — notably
    /// the rollback scope-availability gate — so a partial OIDC publish can
    /// still yank, even though no ambient token exists. The token is also
    /// retained as a marker so a later `rollback()` knows a minted token is
    /// live and must be revoked after the yank.
    ///
    /// Paired with [`Context::end_cargo_trusted_publishing`], which restores
    /// the base source and returns the token for best-effort revocation.
    pub fn begin_cargo_trusted_publishing(&mut self, token: String) {
        let base = self.env_source_arc();
        self.env_source = Arc::new(crate::env_source::LayeredEnvSource::new(
            Arc::clone(&base),
            [("CARGO_REGISTRY_TOKEN".to_string(), token.clone())],
        ));
        self.cargo_trusted_publishing = Some(CargoTrustedPublishing { token, base });
        self.refresh_secret_env();
    }

    /// The minted crates.io Trusted-Publishing token, if an overlay is active.
    /// `rollback()` reads this to learn (i) that the yank must inject a minted
    /// token, and (ii) that the token must be revoked once the yank completes.
    pub fn cargo_trusted_publishing_token(&self) -> Option<&str> {
        self.cargo_trusted_publishing
            .as_ref()
            .map(|s| s.token.as_str())
    }

    /// Tear down the Trusted-Publishing overlay: restore the captured base env
    /// source, drop the marker, and return the minted token so the caller can
    /// revoke it (best-effort). Returns `None` when no overlay is active (the
    /// `auth: token` / ambient path never mints, so its long-lived token is
    /// neither overlaid nor revoked).
    pub fn end_cargo_trusted_publishing(&mut self) -> Option<String> {
        let state = self.cargo_trusted_publishing.take()?;
        self.env_source = state.base;
        self.refresh_secret_env();
        Some(state.token)
    }

    /// Borrow the injected environment-variable source as a trait
    /// object so callers can pass it into helpers that take
    /// `&dyn EnvSource` / `&E: EnvSource + ?Sized` without re-binding
    /// each var through [`Context::env_var`].
    pub fn env_source(&self) -> &dyn EnvSource {
        self.env_source.as_ref()
    }

    /// Clone the injected environment-variable source as an `Arc` so
    /// callers can move it into a `tokio::spawn` future or any other
    /// `'static` closure. Production-default value is
    /// [`ProcessEnvSource`]; tests may replace it via
    /// [`Context::set_env_source`].
    pub fn env_source_arc(&self) -> Arc<dyn EnvSource> {
        Arc::clone(&self.env_source)
    }

    /// Attach an in-memory log-capture sink so every logger derived from
    /// this context via [`Context::logger`] records to it. Intended for
    /// tests; production callers leave this `None`.
    ///
    /// Gated behind the `test-helpers` Cargo feature.
    #[cfg(feature = "test-helpers")]
    pub fn with_log_capture(&mut self, capture: crate::log::LogCapture) {
        self.log_capture = Some(capture);
    }

    /// Build the env-pairs list used to seed every [`StageLogger`] created
    /// via [`Context::logger`]. Combines the template-engine env map
    /// (config env + `.env` file values) with the injected [`EnvSource`]'s
    /// full snapshot ([`EnvSource::vars`]), deduplicating by key
    /// (template-engine values win because they reflect any user
    /// overrides).
    ///
    /// Routes through `self.env_source` — not a raw `std::env::vars()` read
    /// — so [`TestContextBuilder::sealed_env`](crate::test_helpers::TestContextBuilder::sealed_env)'s
    /// documented "never the ambient process environment" promise also
    /// covers log/announce redaction, not just [`Context::env_var`] point
    /// lookups. A hermetic test that seals its env must not have an
    /// unrelated real ambient secret-suffixed var silently mask a literal
    /// fixture substring in the redacted output.
    fn env_for_redact(&self) -> Vec<(String, String)> {
        use std::collections::HashMap;
        let mut map: HashMap<String, String> = self.env_source.vars().into_iter().collect();
        for (k, v) in self.template_vars.all_env() {
            map.insert(k.clone(), v.clone());
        }
        map.into_iter().collect()
    }

    /// Recompute [`Context::env_for_redact`] and publish it into
    /// [`Context::secret_env`], the live cell every [`StageLogger`] produced
    /// by [`Context::logger`] shares. Called at every `env_source` mutation
    /// point so a logger built earlier in the run still redacts a secret
    /// minted afterward (see the `secret_env` field doc for the concrete
    /// crates.io Trusted-Publishing scenario this closes).
    fn refresh_secret_env(&self) {
        let fresh = self.env_for_redact();
        *self.secret_env.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
    }
}

/// Set the full version-derived template var block (`Tag`, `Version`,
/// `RawVersion`, `Base`, `Major`, `Minor`, `Patch`, `Prerelease`,
/// `BuildMetadata`) from a parsed `semver` and the release `tag`. The single
/// source of truth for this block, shared by [`Context::populate_git_vars`] (the
/// context's own git version) and [`Context::render_template_for_version`] (a
/// promotion's target version) so the two can never drift.
fn set_version_vars(vars: &mut TemplateVars, semver: &crate::git::SemVer, tag: &str) {
    // RawVersion: major.minor.patch only, no prerelease / build metadata.
    let raw_version = semver.raw_version_string();
    // Version: clean semver derived from the parsed struct (handles every
    // tag_template prefix, e.g. monorepo `core-v0.3.2`).
    let version = semver.version_string();

    vars.set("Tag", tag);
    vars.set("Version", &version);
    vars.set("RawVersion", &raw_version);
    // `Base`: the numeric base semver, captured before snapshot/nightly version
    // templating overwrites `Version`, for schemes like
    // `"{{ .Base }}-nightly.{{ .NightlyBuild }}+{{ .ShortCommit }}"`.
    vars.set("Base", &raw_version);
    vars.set("Major", &semver.major.to_string());
    vars.set("Minor", &semver.minor.to_string());
    vars.set("Patch", &semver.patch.to_string());
    vars.set("Prerelease", semver.prerelease.as_deref().unwrap_or(""));
    vars.set(
        "BuildMetadata",
        semver.build_metadata.as_deref().unwrap_or(""),
    );
}
