//! Post-publish polling for package-manager publishers whose submission
//! endpoint reports `HTTP 2xx` long before the package is actually visible
//! to end users.
//!
//! Two flavours of "submission ≠ live" exist:
//!
//! - **Chocolatey moderation queue.** `choco push` is accepted by
//!   `push.chocolatey.org` immediately, but the package sits in a
//!   human-moderator queue until approved. Until then it does not appear
//!   in default `choco search` results and `choco install` refuses to
//!   install it. The user-visible signal lives on
//!   `https://community.chocolatey.org/packages/<name>/<version>` in the
//!   moderation callout block.
//!
//! - **WinGet PR validation.** A push to a fork branch + a PR against
//!   `microsoft/winget-pkgs` runs the upstream validation pipeline
//!   (Azure DevOps), which can take 30s–30min. Labels like
//!   `Validation-Installation-Error`, `Validation-Hash-Verification-Failed`
//!   or `Needs-Author-Feedback` signal rejection; `Moderator-Approved` +
//!   merged is the terminal success.
//!
//! The publish step's own success criterion (HTTP 2xx from the submission
//! endpoint) is preserved unchanged. Polling is an *additional* signal
//! attached to the per-publisher result, consumed by the release-summary
//! renderer.
//!
//! Concurrency: when both publishers have polling enabled, polling runs
//! in parallel threads so a 30min wait + a 30min wait completes in 30min
//! rather than 1hr.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anodizer_core::config::PostPublishPollConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

pub mod chocolatey;
mod status;
pub mod winget;

#[cfg(test)]
mod tests;

pub use status::{PostPublishResult, PostPublishStatus};

/// Wall-clock ceiling for a single burn-detection probe: the published-state
/// guard probing many packages before a destructive rollback must stay
/// bounded, so the retry ladder is cut short once this budget is spent.
const BURN_PROBE_DEADLINE: Duration = Duration::from_secs(120);

/// One deadline-bounded GET on the shallow guard-probe retry ladder, shared
/// by the burn-detection probes so each keeps only its URL construction and
/// response classification. `decorate` adds per-registry request headers
/// (auth, API version); `success_class` decides whether a 3xx is a
/// resolvable answer handed to the classifier or a non-success status.
fn burn_probe_get(
    label: &str,
    client: &reqwest::blocking::Client,
    url: &str,
    decorate: impl Fn(reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder,
    success_class: anodizer_core::retry::SuccessClass,
    error_msg: impl Fn(reqwest::StatusCode, &str) -> String,
    log: &StageLogger,
) -> anyhow::Result<(reqwest::StatusCode, String)> {
    use anodizer_core::retry::{RetryLog, RetryPolicy, retry_http_blocking_deadline};
    let deadline = std::time::Instant::now() + BURN_PROBE_DEADLINE;
    retry_http_blocking_deadline(
        RetryLog::new(label, log),
        &RetryPolicy::GUARD_PROBE,
        Some(deadline),
        success_class,
        |_| decorate(client.get(url)).send(),
        error_msg,
    )
}

/// Resolve effective polling config: merge the per-publisher block with
/// the `--no-post-publish-poll` global override.
///
/// Returns `None` when polling is disabled (either via config or CLI flag),
/// `Some(cfg)` when the poller should run.
pub(crate) fn resolve_poll_config(
    ctx: &Context,
    publisher_cfg: Option<PostPublishPollConfig>,
) -> Option<PostPublishPollConfig> {
    if ctx.options.skip_post_publish_poll {
        return None;
    }
    let cfg = publisher_cfg.unwrap_or_default();
    if !cfg.enabled {
        return None;
    }
    Some(cfg)
}

/// One unit of work for [`run_post_publish_polls`].
pub enum PollJob {
    Chocolatey {
        package: String,
        version: String,
        page_base_url: String,
        cfg: PostPublishPollConfig,
    },
    Winget {
        package_identifier: String,
        version: String,
        api_base_url: String,
        /// `<owner>/<repo>` the manifest PR targets — the same upstream the
        /// publisher submitted to.
        upstream_slug: String,
        /// Whether the PR search may keep GitHub's `in:title` qualifier
        /// (false when a custom `commit_msg_template` makes the title
        /// unpredictable).
        search_in_title: bool,
        token: Option<String>,
        cfg: PostPublishPollConfig,
    },
}

impl PollJob {
    fn label(&self) -> &'static str {
        match self {
            PollJob::Chocolatey { .. } => "chocolatey",
            PollJob::Winget { .. } => "winget",
        }
    }

    fn package(&self) -> &str {
        match self {
            PollJob::Chocolatey { package, .. } => package,
            PollJob::Winget {
                package_identifier, ..
            } => package_identifier,
        }
    }

    fn version(&self) -> &str {
        match self {
            PollJob::Chocolatey { version, .. } => version,
            PollJob::Winget { version, .. } => version,
        }
    }

    fn execute(self, log: &StageLogger) -> PostPublishStatus {
        match self {
            PollJob::Chocolatey {
                package,
                version,
                page_base_url,
                cfg,
            } => chocolatey::poll(&page_base_url, &package, &version, cfg, log),
            PollJob::Winget {
                package_identifier,
                version,
                api_base_url,
                upstream_slug,
                search_in_title,
                token,
                cfg,
            } => winget::poll(
                &api_base_url,
                &winget::PollTarget {
                    upstream_slug,
                    package_identifier,
                    version,
                    search_in_title,
                },
                token.as_deref(),
                cfg,
                log,
            ),
        }
    }
}

/// Run every job in parallel, collecting results in fan-out / fan-in
/// fashion. Order of returned results matches input order.
///
/// Each job runs on its own `std::thread::spawn` worker (no tokio
/// dependency yet for this crate). Worker panics are mapped to
/// `PostPublishStatus::Error` rather than killing the publish stage —
/// the polling result is advisory; the publish step has already reported
/// HTTP 2xx by the time we get here.
pub fn run_post_publish_polls(jobs: Vec<PollJob>, log: &StageLogger) -> Vec<PostPublishResult> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let (tx, rx) = mpsc::channel();
    let n = jobs.len();
    for (idx, job) in jobs.into_iter().enumerate() {
        let tx = tx.clone();
        // `job.label()` returns &'static str — picking the stage label
        // from a fixed match table keeps the StageLogger constructor
        // happy without leaking a per-job format!() string.
        let stage_label: &'static str = match &job {
            PollJob::Chocolatey { .. } => "post-publish:chocolatey",
            PollJob::Winget { .. } => "post-publish:winget",
        };
        let verbosity = log.verbosity();
        let publisher = job.label().to_string();
        let package = job.package().to_string();
        let version = job.version().to_string();
        thread::spawn(move || {
            let worker_log = StageLogger::new(stage_label, verbosity);
            let status = job.execute(&worker_log);
            // Receiver only drops if the parent thread already gave up on
            // collecting results (panic in `run_post_publish_polls` or
            // early-return). Surface that in logs so a missing result in
            // the final report is at least attributable.
            if let Err(send_err) = tx.send((
                idx,
                PostPublishResult {
                    publisher: publisher.clone(),
                    package: package.clone(),
                    version: version.clone(),
                    status,
                },
            )) {
                worker_log.warn(&format!(
                    "post-publish result channel closed for {} {} {}: {}",
                    publisher, package, version, send_err
                ));
            }
        });
    }
    drop(tx);
    let mut buf: Vec<Option<PostPublishResult>> = (0..n).map(|_| None).collect();
    while let Ok((idx, result)) = rx.recv() {
        if let Some(slot) = buf.get_mut(idx) {
            *slot = Some(result);
        }
    }
    buf.into_iter()
        .map(|opt| {
            opt.unwrap_or_else(|| PostPublishResult {
                publisher: "unknown".to_string(),
                package: String::new(),
                version: String::new(),
                status: PostPublishStatus::Error {
                    reason: "polling worker dropped without reporting".to_string(),
                },
            })
        })
        .collect()
}

/// Helper for individual pollers: sleep for at most `interval` (or what's
/// left of the polling budget), whichever is smaller. Returns `true` when
/// the caller still has budget; `false` when the timeout has elapsed.
pub(crate) fn sleep_or_timeout(
    elapsed: Duration,
    interval: Duration,
    total_budget: Duration,
) -> bool {
    if elapsed >= total_budget {
        return false;
    }
    let remaining = total_budget - elapsed;
    let nap = std::cmp::min(remaining, interval);
    if nap > Duration::ZERO {
        thread::sleep(nap);
    }
    elapsed + nap < total_budget
}
