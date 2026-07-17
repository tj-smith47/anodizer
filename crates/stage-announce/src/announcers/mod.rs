//! The `Announcer` trait, the single-announcer runner, the per-provider
//! impls (one struct per platform), and the dispatch loop that runs them
//! all and collects per-provider errors.
//!
//! Each impl delegates the actual side effect to the matching per-platform
//! submodule (`crate::discord`, `crate::slack`, …) via the shared
//! [`crate::dispatch::dispatch`] helper; this module owns only the trait
//! wiring and the registration order.

use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use std::time::Duration;

use crate::dispatch::{DispatchOutcome, DispatchQueue, run_queue};

mod validators;

// One submodule per announcer backend; the dispatch table and shared trait
// live in this file.

mod bluesky;
mod discord;
mod discourse;
mod email;
mod linkedin;
mod mastodon;
mod mattermost;
mod opencollective;
mod reddit;
mod slack;
mod teams;
mod telegram;
mod twitter;
mod webhook;

use bluesky::BlueskyAnnouncer;
use discord::DiscordAnnouncer;
use discourse::DiscourseAnnouncer;
use email::EmailAnnouncer;
use linkedin::LinkedInAnnouncer;
use mastodon::MastodonAnnouncer;
use mattermost::MattermostAnnouncer;
use opencollective::OpenCollectiveAnnouncer;
use reddit::RedditAnnouncer;
use slack::SlackAnnouncer;
use teams::TeamsAnnouncer;
use telegram::TelegramAnnouncer;
use twitter::TwitterAnnouncer;
use webhook::WebhookAnnouncer;

// ---------------------------------------------------------------------------
// Announcer trait + dispatch helper
// ---------------------------------------------------------------------------

/// Per-provider announce dispatch.
///
/// `enabled` decides whether the provider's config block is present and
/// the provider opted in (rendering the `enabled:` template if any).
/// `send` performs the side effect; per-provider errors are collected
/// at the call site rather than fast-failing the stage.
trait Announcer: Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool>;
    /// Render this provider's templates (serially, borrowing `&mut ctx`) and
    /// ENQUEUE its pure network action onto `queue` for concurrent dispatch —
    /// it does NOT perform the network send. A render-phase failure (bad
    /// template, missing required field in strict mode) returns `Err` here, on
    /// the serial pass, before anything is queued; the network result is
    /// collected later by [`run_queue`]. The queued closure must own its inputs
    /// (`'static`) so it can run on a detached worker.
    fn send(
        &self,
        ctx: &mut Context,
        announce: &AnnounceConfig,
        retry: &RetryPolicy,
        log: &StageLogger,
        key_width: usize,
        queue: &mut DispatchQueue,
    ) -> Result<()>;

    /// Render — but do not send — exactly the templates this announcer's
    /// [`send`](Announcer::send) would render, so a broken template
    /// (`{{ ReleaseURL }}` typo, undefined var, malformed Tera) surfaces as
    /// an `Err` BEFORE any irreversible publisher fires.
    ///
    /// Reads ZERO credentials/env — only [`send`](Announcer::send) touches
    /// the network and secrets — so the pre-publish guard runs on a CI box
    /// without announce secrets. Each impl must render every template field
    /// `send` renders (`message_template`, `title_template`, `enabled`,
    /// `url`/`icon_url`, …); a field rendered by `send` but skipped here is a
    /// hole in the guard. The default `Ok(())` is overridden per provider.
    fn render_only(&self, _ctx: &mut Context, _announce: &AnnounceConfig) -> Result<()> {
        Ok(())
    }
}

/// Render every active announcer's templates serially (the `&mut ctx`
/// render pass), enqueueing each provider's pure network action, then run the
/// queue CONCURRENTLY under `deadline` and fold the results back into `errors`
/// and the per-version sent-marker.
///
/// Render-phase failures (broken template, missing required field in strict
/// mode) are captured per-provider on the serial pass — before anything is
/// queued — so a bad template never reaches the network. An announcer already
/// recorded in `marker` (a re-run at the same version) is skipped without
/// re-queueing, preserving idempotency. The marker is updated SERIALLY here,
/// after the join, so the shared file write is never touched concurrently.
///
/// `marker` carries the per-version sent-marker on the live path (so re-runs
/// are idempotent) and is `None` on dry-run (which queues nothing).
#[allow(clippy::too_many_arguments)]
fn dispatch_active(
    active: Vec<&'static dyn Announcer>,
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    deadline: Duration,
    errors: &mut Vec<String>,
    marker: Option<&mut crate::sent_marker::AnnounceSentMarker>,
) -> Result<()> {
    let key_width = shared_key_width(&active);
    let mut queue = DispatchQueue::new();

    for a in active {
        // Idempotency gate: a re-run at an already-announced version must not
        // re-post to a channel that already fired.
        if let Some(ref m) = marker
            && m.already_sent(a.name())
        {
            log.status(&format!(
                "skipped {} — already announced this version",
                a.name()
            ));
            continue;
        }
        // Render phase (serial, &mut ctx). A render error is captured per
        // provider; the network action is enqueued for the concurrent runner.
        if let Err(e) = a.send(ctx, announce, retry_policy, log, key_width, &mut queue) {
            // `{e:#}` flattens the anyhow chain into "outer: middle: root" so
            // the summary names the underlying failure (a missing template
            // variable, a wrapped tera syntax error), not just the wrapper.
            errors.push(format!("{}: {e:#}", a.name()));
        }
    }

    // Dry-run queues nothing (the render pass logged `(dry-run)` per provider).
    if queue.is_empty() {
        return Ok(());
    }

    let DispatchOutcome {
        errors: send_errors,
        abandoned,
        succeeded,
    } = run_queue(queue, deadline);

    for (_, msg) in send_errors {
        errors.push(msg);
    }
    // A channel still running at the deadline is a best-effort straggler: warn
    // and abandon rather than fail an already-published release.
    for provider in &abandoned {
        let secs = deadline.as_secs();
        log.warn(&format!(
            "announce {provider} did not complete within the {secs}s stage deadline; abandoned"
        ));
    }
    // Record successful sends serially, after the join, so the shared marker
    // file is never written concurrently.
    if let Some(m) = marker {
        for provider in &succeeded {
            m.mark_sent(provider, log);
        }
    }

    Ok(())
}

/// Dispatch every registered announcer, collecting per-provider errors.
///
/// `deadline` bounds the concurrent send pass; stragglers are abandoned with a
/// warning. `marker` carries the per-version sent-marker on the live path (so
/// re-runs are idempotent) and is `None` on dry-run.
pub(crate) fn dispatch_all_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    deadline: Duration,
    errors: &mut Vec<String>,
    marker: Option<&mut crate::sent_marker::AnnounceSentMarker>,
) -> Result<()> {
    let active = enabled_announcers(ctx, announce, None)?;
    dispatch_active(
        active,
        ctx,
        announce,
        retry_policy,
        log,
        deadline,
        errors,
        marker,
    )
}

/// Resolve the announcers that will actually fire: apply the name
/// filter (when given), then evaluate each `enabled:` template exactly
/// once. A broken `enabled:` template aborts HERE, before any announcer
/// sends — a half-dispatched section (earlier channels posted, later
/// ones dead) is worse than failing fast with nothing sent.
fn enabled_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    filter: Option<&AnnounceFilter<'_>>,
) -> Result<Vec<&'static dyn Announcer>> {
    let mut active: Vec<&'static dyn Announcer> = Vec::new();
    for announcer in announcer_registry() {
        let name = announcer.name();
        if let Some(f) = filter
            && (f.include.is_some_and(|inc| !inc.contains(&name)) || f.skip.contains(&name))
        {
            continue;
        }
        if announcer.enabled(ctx, announce)? {
            active.push(*announcer);
        }
    }
    Ok(active)
}

/// Widest `name()` among the announcers that will fire, so every
/// provider kv row in one Announcing section pads to the same column.
fn shared_key_width(active: &[&'static dyn Announcer]) -> usize {
    active
        .iter()
        .map(|a| a.name().chars().count())
        .max()
        .unwrap_or(0)
}

/// Filter descriptor for [`dispatch_filtered_announcers`].
pub(crate) struct AnnounceFilter<'a> {
    /// When `Some`, only announcers whose `name()` appears here are fired.
    /// `None` means all announcers are eligible.
    pub include: Option<&'a [&'a str]>,
    /// Announcers whose `name()` appears here are skipped regardless of
    /// `include`.
    pub skip: &'a [&'a str],
}

/// Like [`dispatch_all_announcers`] but filters by integration name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_filtered_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    deadline: Duration,
    errors: &mut Vec<String>,
    marker: Option<&mut crate::sent_marker::AnnounceSentMarker>,
    filter: AnnounceFilter<'_>,
) -> Result<()> {
    let active = enabled_announcers(ctx, announce, Some(&filter))?;
    dispatch_active(
        active,
        ctx,
        announce,
        retry_policy,
        log,
        deadline,
        errors,
        marker,
    )
}

/// The registered announcer set, in dispatch order. Single source of truth for
/// both [`dispatch_all_announcers`] (which sends) and [`render_all_announcers`]
/// (which only renders), so the pre-publish guard exercises exactly the set the
/// real announce path would.
fn announcer_registry() -> &'static [&'static dyn Announcer] {
    &[
        &DiscordAnnouncer,
        &DiscourseAnnouncer,
        &SlackAnnouncer,
        &WebhookAnnouncer,
        &TelegramAnnouncer,
        &TeamsAnnouncer,
        &MattermostAnnouncer,
        &RedditAnnouncer,
        &TwitterAnnouncer,
        &MastodonAnnouncer,
        &BlueskyAnnouncer,
        &LinkedInAnnouncer,
        &OpenCollectiveAnnouncer,
        &EmailAnnouncer,
    ]
}

/// The names of every announcer whose config block is present, for the
/// non-release version guard's error message. Presence-based (not `enabled:`
/// template evaluation) so it is side-effect-free and can run BEFORE any
/// dispatch — it only needs to name the channels a snapshot version was about
/// to broadcast to, not the exact final enabled set.
pub(crate) fn configured_announcer_names(announce: &AnnounceConfig) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |present: bool, name: &str| {
        if present {
            names.push(name.to_string());
        }
    };
    push(announce.discord.is_some(), "discord");
    push(announce.discourse.is_some(), "discourse");
    push(announce.slack.is_some(), "slack");
    push(announce.webhook.is_some(), "webhook");
    push(announce.telegram.is_some(), "telegram");
    push(announce.teams.is_some(), "teams");
    push(announce.mattermost.is_some(), "mattermost");
    push(announce.reddit.is_some(), "reddit");
    push(announce.twitter.is_some(), "twitter");
    push(announce.mastodon.is_some(), "mastodon");
    push(announce.bluesky.is_some(), "bluesky");
    push(announce.linkedin.is_some(), "linkedin");
    push(announce.opencollective.is_some(), "opencollective");
    push(announce.email.is_some(), "email");
    names
}

/// Dry-render every ENABLED announcer's templates, collecting a per-provider
/// error (`"<provider>: <chain>"`) for any that fail to render. Sends nothing
/// and reads no credentials — the pre-publish guard's announce half.
///
/// An announcer whose `enabled` template is falsy (or whose config block is
/// absent) is skipped, matching the real dispatch loop, so the guard never
/// flags a provider the live run would not touch.
pub(crate) fn render_all_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    errors: &mut Vec<String>,
) -> Result<()> {
    for announcer in announcer_registry() {
        if !announcer.enabled(ctx, announce)? {
            continue;
        }
        if let Err(e) = announcer.render_only(ctx, announce) {
            errors.push(format!("{}: {e:#}", announcer.name()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
