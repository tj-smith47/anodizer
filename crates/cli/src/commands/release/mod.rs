mod announce_only;
mod context_setup;
mod crate_select;
mod milestones;
mod on_error;
mod pipeline_run;
mod publish_only;
mod run;
mod split;

pub(crate) use context_setup::*;
pub(crate) use crate_select::*;
pub(crate) use pipeline_run::*;
pub use run::run;
pub use split::{load_split_contexts_into, run_merge};

use super::helpers;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

pub struct ReleaseOpts {
    pub crate_names: Vec<String>,
    pub all: bool,
    pub force: bool,
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub clean: bool,
    pub skip: Vec<String>,
    /// `--publishers`: per-publisher allowlist (empty = all configured run).
    /// Flows to [`ContextOptions::publisher_allowlist`]; the dispatch loop
    /// deselects any publisher not listed. `--skip` always wins.
    pub publishers: Vec<String>,
    pub token: Option<String>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub config_override: Option<PathBuf>,
    pub parallelism: usize,
    pub single_target: Option<String>,
    /// `--targets=<csv>`: restrict the build to a comma-separated subset
    /// of configured target triples. Used by the sharded Determinism
    /// Harness (each runner only validates its own native targets) and
    /// available to operators driving custom CI matrices. When `Some`,
    /// the release dispatcher populates
    /// `ContextOptions::partial_target = Some(PartialTarget::Targets(...))`
    /// so the existing build-stage filter (`partial.filter_targets`)
    /// trims the configured list down to the intersection. Mutually
    /// exclusive with `single_target` (clap-level `conflicts_with`).
    pub targets: Option<Vec<String>>,
    /// `--host-targets`: build every configured target this host can build,
    /// skipping only the ones that need a cross-toolchain anodizer doesn't
    /// have (apple targets off a non-macOS host; windows-msvc targets off a
    /// non-Windows host — both need a native SDK cargo-zigbuild can't supply;
    /// `*-windows-gnu` and linux targets cross-build from any host). Resolved
    /// into the same
    /// `targets` intersection-filter at the top of [`run`]: the configured
    /// target union is partitioned via
    /// [`anodizer_core::partial::host_buildable_targets`], the skipped set is
    /// logged once, and the kept set is fed through the existing
    /// `PartialTarget::Targets` plumbing. Gated to snapshot / dry-run at the
    /// CLI layer so a real release can never ship an incomplete target set.
    pub host_targets: bool,
    pub release_notes: Option<PathBuf>,
    pub release_notes_tmpl: Option<PathBuf>,
    pub workspace: Option<String>,
    pub draft: bool,
    pub release_header: Option<PathBuf>,
    pub release_header_tmpl: Option<PathBuf>,
    pub release_footer: Option<PathBuf>,
    pub release_footer_tmpl: Option<PathBuf>,
    pub fail_fast: bool,
    pub split: bool,
    pub merge: bool,
    /// `--publish-only`: load `dist/context.json` (preserved by
    /// `anodize check determinism --preserve-dist=...`) and run only
    /// the sign + publish pipeline. Mutually exclusive with `split` /
    /// `merge` at the clap level.
    pub publish_only: bool,
    pub strict: bool,
    /// `--prepare`: run local build/archive/sign/checksum/sbom
    /// stages but NOT release/publish/announce. Implemented by augmenting `skip` with
    /// those three stages at the top of `run()`; artifacts still land under `dist/`.
    pub prepare: bool,
    /// `--announce-only`: re-fire the announce stage after loading a
    /// prior run's `<dist>/run-<id>/report.json`. Use case: a
    /// transient announcer failure (Slack 502, Discord 5xx) after a
    /// successful publish — operator wants to retry notifications
    /// without re-creating the GitHub release or re-uploading
    /// archives. Skips every other stage in the pipeline.
    pub announce_only: bool,
    /// `--resume-release`: continue into an existing release rather than
    /// bailing on the leftover-assets pre-check. Plumbed into
    /// `ContextOptions::resume_release`.
    pub resume_release: bool,
    /// `--replace-existing`: CLI override for `release.replace_existing_artifacts: true`.
    /// Plumbed into `ContextOptions::replace_existing_artifacts`.
    pub replace_existing: bool,
    /// `--preflight`: run the pre-flight publisher-state check and exit
    /// (don't continue into the rest of the release pipeline).
    pub preflight: bool,
    /// `--no-env-preflight` (hidden, harness-only): skip the environment
    /// preflight (tools / secrets / key material) that normally runs as the
    /// first step of `release`. Set by the determinism harness, whose
    /// hermetic replica runs in a deliberately credential-less env that the
    /// config-derived preflight would correctly reject.
    pub no_env_preflight: bool,
    /// `--preflight-secrets`: a check-only mode that validates the
    /// runner-agnostic publish secrets / credentials (env vars and
    /// env-borne key material) across the full release surface WITHOUT
    /// checking host-local tools, then exits with zero mutations. Intended
    /// as a central pre-tag gate ahead of decoupled CI runners that all
    /// carry the same injected secrets but different host-local tools.
    /// Short-circuits before the publisher-state probe and mode dispatch.
    pub preflight_secrets: bool,
    /// `--no-post-publish-poll`: skip the post-publish polling that
    /// otherwise waits on chocolatey moderation / winget PR validation
    /// after the publish step's HTTP 2xx. Plumbed into
    /// `ContextOptions::skip_post_publish_poll`.
    pub no_post_publish_poll: bool,
    /// `--no-gate-submitter`: disable the Submitter gate so Submitter
    /// publishers dispatch even when a required Assets/Manager publisher
    /// failed, OR when the pre-submitter verify-release gate
    /// (`ctx.verify_gate`, installed by this command) did not pass —
    /// [`ensure_verify_gate_evaluated`](anodizer_core::publish_report::ensure_verify_gate_evaluated)
    /// is only ever consulted when `gate_submitter` is true, so this one
    /// flag disables both gates together. Plumbed into
    /// `ContextOptions::gate_submitter` as `Some(false)`. Default
    /// (`None`) means gate-on.
    pub no_gate_submitter: bool,
    /// `--simulate-failure=<publisher>` (repeatable): names of
    /// publishers whose `run()` should be replaced with a synthetic
    /// failure in `stage-publish::dispatch`. Only honored when
    /// `ANODIZE_TEST_HARNESS=1` is set; otherwise rejected at the
    /// translation site so production releases cannot trip it.
    pub simulate_failure: Vec<String>,
    /// `--show-skipped`: surface the per-crate "no `<publisher>` config
    /// block" skip lines at default verbosity. Plumbed into
    /// `ContextOptions::show_skipped`; defaults to false (those no-op skips
    /// route to debug so workspace mode does not emit one line per
    /// non-applicable crate per publisher).
    pub show_skipped: bool,
    /// `--allow-nondeterministic <name>=<reason>` (repeatable):
    /// runtime non-determinism opt-outs. Parsed at the translation
    /// site into `(name, reason)` tuples; empty reasons are rejected
    /// so the report always carries a human-readable justification.
    pub allow_nondeterministic: Vec<String>,
    /// `--summary-json=<path>`: when set, the per-publisher run
    /// summary is written here.
    pub summary_json: Option<PathBuf>,
    /// `--allow-ai-failure`: opt-in to degraded behaviour when
    /// `changelog.ai` is configured and the provider fails. Default
    /// (fail-closed) aborts the release on any provider error so the
    /// operator notices instead of shipping the pre-AI body silently.
    pub allow_ai_failure: bool,
    /// `--allow-snapshot-publish`: downgrade the publish, blob, and announce
    /// stages' non-release version guard from a hard bail to a warning, allowing
    /// a snapshot / dirty / `0.0.0`-sentinel version to be released. Default
    /// `false` (fail-closed): a non-release version reaching a one-way-door
    /// index is almost always an accident.
    pub allow_snapshot_publish: bool,
}

#[cfg(test)]
mod tests;
