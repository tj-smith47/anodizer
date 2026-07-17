use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use super::validators::validate_telegram_thread_id;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message_with_default};
use crate::telegram;

use super::Announcer;

/// Telegram's default message template. MarkdownV2 parse mode requires each
/// dynamic value to pass through the `mdv2escape` filter and literal `!` to be
/// backslash-escaped; the rendered output is byte-equivalent to the upstream
/// `{{ print … | mdv2escape }}` Go-template form. Shared by `send` and
/// `render_only` so the pre-publish guard exercises the exact default `send`
/// would use. Pinned by `test_telegram_default_template_renders_without_tilde`.
const TELEGRAM_DEFAULT_TEMPLATE: &str = "{{ ProjectName | mdv2escape }} {{ Tag | mdv2escape }} is out\\! Check it out at {{ ReleaseURL | mdv2escape }}";

pub(super) struct TelegramAnnouncer;
impl Announcer for TelegramAnnouncer {
    fn name(&self) -> &'static str {
        "telegram"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.telegram {
            Some(cfg) => is_enabled(ctx, cfg.enabled.as_ref()),
            None => Ok(false),
        }
    }
    fn send(
        &self,
        ctx: &mut Context,
        announce: &AnnounceConfig,
        retry_policy: &RetryPolicy,
        log: &StageLogger,
        key_width: usize,
        queue: &mut DispatchQueue,
    ) -> Result<()> {
        let cfg = announce
            .telegram
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let bot_token = match cfg.bot_token.as_deref() {
            Some(t) => ctx.render_template(t)?,
            None => match ctx.env_var("TELEGRAM_TOKEN").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    // Skip-when-empty UX: warn-and-skip in normal mode,
                    // bail in strict mode.
                    ctx.strict_guard(
                        log,
                        "missing bot_token in announce.telegram (set config or TELEGRAM_TOKEN env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let chat_id = match cfg.chat_id.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing chat_id in announce.telegram")?;
                return Ok(());
            }
        };
        // Telegram defaults to MarkdownV2 parse mode, so the default
        // message template must apply the mdv2escape filter.
        //
        // The upstream telegram template uses Go-template syntax:
        //   `{{ print .ProjectName " " .Tag " is out! ... " .ReleaseURL | mdv2escape }}`
        // anodizer renders via Tera. Previously this template used
        // Tera's `~` concatenation operator (`{{ A ~ " " ~ B | filter }}`)
        // — which works, but is hostile to copy-paste: a user pulling
        // the default into a custom template tends to mix it with
        // `print` blocks (Tera then refuses to parse `print`)
        // or rewrite the `~` and break the filter pipeline.
        //
        // The new form uses one `mdv2escape` filter per dynamic value
        // plus pre-escaped literal text (`is out\!` — `!` must be
        // backslash-escaped in MarkdownV2 per the Telegram docs). The
        // rendered output is byte-equivalent to the upstream
        // `{{ print … | mdv2escape }}` form, but the template itself
        // is `{{ … }}`-only and copy-pastes cleanly into custom
        // user templates. Pinned by
        // `test_telegram_default_template_renders_without_tilde`.
        let message = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            TELEGRAM_DEFAULT_TEMPLATE,
        )?;
        // Default parse_mode to "MarkdownV2".
        // Validate against known values; default to MarkdownV2 with a warning for unknowns.
        let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
        let parse_mode_validated = match parse_mode_raw {
            "MarkdownV2" | "HTML" => parse_mode_raw,
            other => {
                log.warn(&format!(
                    "telegram parse_mode {:?} unknown, defaulting to \"MarkdownV2\"",
                    other
                ));
                "MarkdownV2"
            }
        };
        let parse_mode = ctx.render_template_opt(Some(parse_mode_validated))?;
        // message_thread_id is now a String supporting template expressions;
        // render and parse to i64 at runtime.
        let message_thread_id: Option<i64> = match cfg.message_thread_id.as_deref() {
            Some(raw) => {
                let rendered = ctx.render_template(raw)?;
                validate_telegram_thread_id(&rendered)?
            }
            None => None,
        };

        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "telegram",
            message.clone(),
            key_width,
            move || {
                telegram::send_telegram(
                    &bot_token,
                    &chat_id,
                    &message,
                    parse_mode.as_deref(),
                    message_thread_id,
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.telegram.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.bot_token.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.chat_id.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            TELEGRAM_DEFAULT_TEMPLATE,
        )?;
        let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
        let parse_mode_validated = match parse_mode_raw {
            "MarkdownV2" | "HTML" => parse_mode_raw,
            _ => "MarkdownV2",
        };
        ctx.render_template_opt(Some(parse_mode_validated))?;
        if let Some(raw) = cfg.message_thread_id.as_deref() {
            let rendered = ctx.render_template(raw)?;
            validate_telegram_thread_id(&rendered)?;
        }
        Ok(())
    }
}
