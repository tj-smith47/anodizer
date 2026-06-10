//! `anodizer notify` command.
//! Sends a notification through configured announce integrations.

use anyhow::Result;

use anodizer_core::context::{Context, ContextOptions};
use anodizer_stage_announce::dispatch_filtered_announcers;

use super::helpers;
use anodizer_core::log::{StageLogger, Verbosity};

pub struct NotifyOpts {
    pub message: String,
    /// If Some, only fire these integration names.
    pub publishers: Option<Vec<String>>,
    /// Integration names to omit.
    pub skip: Vec<String>,
    pub config_override: Option<std::path::PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub dry_run: bool,
}

pub fn run(opts: NotifyOpts) -> Result<()> {
    let log = StageLogger::new(
        "notify",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        ..Default::default()
    };

    let (_config, mut ctx) =
        helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;

    run_with_ctx(&mut ctx, opts.message, opts.publishers, opts.skip)
}

/// Inner dispatch — takes an already-constructed `Context` so tests can drive
/// it directly without touching the filesystem / git.
pub(crate) fn run_with_ctx(
    ctx: &mut Context,
    message: String,
    publishers: Option<Vec<String>>,
    skip: Vec<String>,
) -> Result<()> {
    // Render the message template up front so a broken template surfaces
    // before any network call fires. The rendered text is then injected into
    // every enabled provider's `message_template`, overriding the per-provider
    // default so all integrations send the same operator-supplied message.
    let rendered = ctx.render_template(&message)?;

    let mut announce = match ctx.config.announce.as_ref() {
        Some(a) => a.clone(),
        None => anyhow::bail!("notify: no announce config found"),
    };

    // Override each provider's message_template with the already-rendered
    // text. Using the literal rendered string (not a Tera expression) means
    // the providers' own render pass will return it verbatim — no double-
    // rendering of Tera syntax that may be present in the user's message.
    inject_message(&mut announce, &rendered);

    let retry_policy = ctx.retry_policy();
    let log = ctx.logger("notify");

    let include_refs: Option<Vec<&str>> = publishers
        .as_deref()
        .map(|v| v.iter().map(|s| s.as_str()).collect());
    let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();

    let mut errors: Vec<String> = Vec::new();
    dispatch_filtered_announcers(
        ctx,
        &announce,
        &retry_policy,
        &log,
        &mut errors,
        include_refs.as_deref(),
        &skip_refs,
    )?;

    if !errors.is_empty() {
        anyhow::bail!(
            "notify: {} integration(s) failed:\n{}",
            errors.len(),
            errors.join("\n")
        );
    }

    Ok(())
}

/// Override every configured provider's `message_template` with `msg` so all
/// integrations dispatch the same operator-supplied text.
///
/// Only providers that are already configured (their config block is `Some`)
/// are updated — this preserves the `enabled` / credential fields the provider
/// needs while replacing only the body text.
fn inject_message(announce: &mut anodizer_core::config::AnnounceConfig, msg: &str) {
    macro_rules! set_msg {
        ($field:expr) => {
            if let Some(ref mut cfg) = $field {
                cfg.message_template = Some(msg.to_owned());
            }
        };
    }
    set_msg!(announce.discord);
    set_msg!(announce.discourse);
    set_msg!(announce.slack);
    set_msg!(announce.webhook);
    set_msg!(announce.telegram);
    set_msg!(announce.teams);
    set_msg!(announce.mattermost);
    set_msg!(announce.email);
    // Reddit uses title_template + url_template rather than message_template;
    // notify dispatches it with the configured templates unchanged.
    set_msg!(announce.twitter);
    set_msg!(announce.mastodon);
    set_msg!(announce.bluesky);
    set_msg!(announce.linkedin);
    set_msg!(announce.opencollective);
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;

    fn minimal_ctx() -> Context {
        let config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    #[test]
    fn no_announce_config_bails() {
        let mut ctx = minimal_ctx();
        let err = run_with_ctx(&mut ctx, "hello".to_string(), None, vec![])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no announce config found"), "{err}");
    }

    #[test]
    fn publishers_none_means_all() {
        let opts_publishers: Option<Vec<String>> = None;
        // None means all — verify the include_refs mapping is correct.
        let include_refs: Option<Vec<&str>> = opts_publishers
            .as_deref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());
        assert!(include_refs.is_none());
    }

    #[test]
    fn publishers_some_maps_to_include_refs() {
        let publishers: Option<Vec<String>> =
            Some(vec!["slack".to_string(), "discord".to_string()]);
        let include_refs: Option<Vec<&str>> = publishers
            .as_deref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());
        assert_eq!(include_refs, Some(vec!["slack", "discord"]));
    }

    #[test]
    fn skip_maps_to_skip_refs() {
        let skip = ["webhook".to_string()];
        let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
        assert_eq!(skip_refs, ["webhook"]);
    }

    #[test]
    fn notify_opts_roundtrip() {
        let opts = NotifyOpts {
            message: "test {{ ProjectName }}".to_string(),
            publishers: Some(vec!["slack".to_string()]),
            skip: vec!["discord".to_string()],
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            dry_run: true,
        };
        assert_eq!(opts.skip.len(), 1);
        assert_eq!(opts.skip[0], "discord");
        assert!(opts.publishers.is_some());
    }

    #[test]
    fn inject_message_sets_all_message_templates() {
        use anodizer_core::config::{
            AnnounceConfig, BlueskyAnnounce, DiscordAnnounce, DiscourseAnnounce, EmailAnnounce,
            LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce, OpenCollectiveAnnounce,
            SlackAnnounce, TeamsAnnounce, TelegramAnnounce, TwitterAnnounce, WebhookConfig,
        };
        let mut announce = AnnounceConfig {
            discord: Some(DiscordAnnounce::default()),
            discourse: Some(DiscourseAnnounce::default()),
            slack: Some(SlackAnnounce::default()),
            webhook: Some(WebhookConfig::default()),
            telegram: Some(TelegramAnnounce::default()),
            teams: Some(TeamsAnnounce::default()),
            mattermost: Some(MattermostAnnounce::default()),
            email: Some(EmailAnnounce::default()),
            twitter: Some(TwitterAnnounce::default()),
            mastodon: Some(MastodonAnnounce::default()),
            bluesky: Some(BlueskyAnnounce::default()),
            linkedin: Some(LinkedInAnnounce::default()),
            opencollective: Some(OpenCollectiveAnnounce::default()),
            ..Default::default()
        };
        inject_message(&mut announce, "hello world");
        assert_eq!(
            announce.discord.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        assert_eq!(
            announce.slack.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        assert_eq!(
            announce.teams.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        // reddit is intentionally skipped — no message_template field.
        assert!(announce.reddit.is_none());
    }

    #[test]
    fn dispatch_with_empty_announce_config_succeeds() {
        use anodizer_core::config::AnnounceConfig;
        let config = anodizer_core::config::Config {
            project_name: "test".to_string(),
            announce: Some(AnnounceConfig::default()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        // No providers configured → dispatch fires nothing, returns Ok.
        let result = run_with_ctx(&mut ctx, "hello".to_string(), None, vec![]);
        assert!(result.is_ok(), "{result:?}");
    }
}
