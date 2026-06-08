//! Pre-publish announce template dry-render.
//!
//! Announcers render `{{ ReleaseURL }}`-class templates only after the release
//! and every publisher have run, so a broken announce template (an undefined
//! variable, a malformed Tera expression) historically surfaced AFTER an
//! irreversible publisher (chocolatey/winget moderation, AUR push) had already
//! fired. [`validate_announce_templates`] closes that window: it renders every
//! enabled announcer's templates against the real context — with the real /
//! derived `ReleaseURL` already in scope — sending nothing and reading no
//! credentials, and fails loud listing every announcer whose templates break.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Result, bail};

use crate::announcers::render_all_announcers;
use crate::run::announce_should_run;

/// Dry-render every enabled announcer's templates, failing loud if any break.
///
/// No-op in snapshot / nightly (where no announcer dispatches) and when no
/// `announce:` config is present. Mirrors the real announce path's gating
/// ([`announce_should_run`] for `skip`/`if`/`gate_on`) so it never flags an
/// announcer the live run would skip. Renders the SAME template fields each
/// announcer's `send` renders, reading zero credentials/env, so it runs on a CI
/// box without announce secrets. On any render failure it `bail!`s once,
/// naming every broken announcer.
pub fn validate_announce_templates(ctx: &mut Context, log: &StageLogger) -> Result<()> {
    if ctx.is_snapshot() || ctx.is_nightly() {
        return Ok(());
    }

    let announce = match ctx.config.announce.clone() {
        Some(a) => a,
        None => return Ok(()),
    };

    // Match the live path: announce templates can iterate `Artifacts`, so the
    // var must be populated before rendering or a `{{ Artifacts }}` reference
    // renders against a stale set.
    ctx.refresh_artifacts_var();

    if !announce_should_run(ctx, &announce)? {
        log.verbose("prepublish-guard: announce gated/skipped — no templates to dry-render");
        return Ok(());
    }

    let mut errors: Vec<String> = vec![];
    render_all_announcers(ctx, &announce, &mut errors)?;

    if !errors.is_empty() {
        bail!("announce template render failed:\n{}", errors.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        AnnounceConfig, Config, DiscordAnnounce, EmailAnnounce, StringOrBool, TeamsAnnounce,
        WebhookConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    fn ctx_with(announce: AnnounceConfig, opts: ContextOptions) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            announce: Some(announce),
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.set_release_url("https://github.com/acme/myapp/releases/tag/v1.2.3");
        ctx
    }

    fn enabled() -> Option<StringOrBool> {
        Some(StringOrBool::Bool(true))
    }

    /// A broken `{{ ReleaseURL }}`-class template (undefined var) in a
    /// message field fails the guard, naming the announcer. The exact
    /// ReleaseURL-bug regression.
    #[test]
    fn broken_message_template_fails_naming_announcer() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ NoSuchVar }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        let err = validate_announce_templates(&mut ctx, &log)
            .expect_err("broken template must fail the guard");
        let msg = format!("{err:#}");
        assert!(msg.contains("discord"), "error names the announcer: {msg}");
    }

    /// A template broken ONLY in a non-message field (`title_template`) is
    /// still caught — protects against the render-only-renders-fewer-fields
    /// hole.
    #[test]
    fn broken_non_message_field_is_caught() {
        let announce = AnnounceConfig {
            teams: Some(TeamsAnnounce {
                enabled: enabled(),
                webhook_url: Some("https://example.com/hook".to_string()),
                message_template: Some("ok {{ Tag }}".to_string()),
                title_template: Some("{{ NoSuchVar }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        let err = validate_announce_templates(&mut ctx, &log)
            .expect_err("broken non-message field must fail the guard");
        assert!(format!("{err:#}").contains("teams"));
    }

    /// Multiple broken announcers surface in one combined error (no
    /// short-circuit after the first).
    #[test]
    fn all_broken_announcers_are_listed() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ BadA }}".to_string()),
                ..Default::default()
            }),
            email: Some(EmailAnnounce {
                enabled: enabled(),
                from: Some("rel@acme.example".to_string()),
                to: vec!["dev@acme.example".to_string()],
                subject_template: Some("{{ BadB }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        let err = validate_announce_templates(&mut ctx, &log).expect_err("both must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("discord"), "lists discord: {msg}");
        assert!(msg.contains("email"), "lists email: {msg}");
    }

    /// All templates valid → guard returns Ok.
    #[test]
    fn valid_templates_pass() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ ProjectName }} {{ Tag }} {{ ReleaseURL }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        validate_announce_templates(&mut ctx, &log).expect("valid templates pass");
    }

    /// A disabled announcer with a broken template is NOT flagged — the guard
    /// mirrors the live dispatch loop's enablement gate.
    #[test]
    fn disabled_announcer_with_broken_template_is_not_flagged() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: Some(StringOrBool::Bool(false)),
                message_template: Some("{{ NoSuchVar }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        validate_announce_templates(&mut ctx, &log)
            .expect("disabled announcer's broken template must not fail the guard");
    }

    /// Snapshot mode short-circuits to Ok even with a broken template, since
    /// no announcer dispatches in snapshot.
    #[test]
    fn snapshot_is_a_noop() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ NoSuchVar }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = ContextOptions {
            snapshot: true,
            ..ContextOptions::default()
        };
        let mut ctx = ctx_with(announce, opts);
        let log = ctx.logger("prepublish-guard");
        validate_announce_templates(&mut ctx, &log).expect("snapshot is a no-op");
    }

    /// A template that RENDERS CLEANLY but produces an invalid VALUE is
    /// caught by the guard — the late-failure the guard exists to prevent.
    /// Discord `color` rendering to a non-numeric string must fail before
    /// any one-way publisher fires, naming the announcer.
    #[test]
    fn invalid_rendered_discord_color_fails_naming_announcer() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ ProjectName }} {{ Tag }}".to_string()),
                // Valid template syntax; renders to the literal "notacolor",
                // which `send`'s color parse rejects.
                color: Some("notacolor".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        let err = validate_announce_templates(&mut ctx, &log)
            .expect_err("invalid rendered color must fail the guard");
        let msg = format!("{err:#}");
        assert!(msg.contains("discord"), "error names the announcer: {msg}");
        assert!(msg.contains("color"), "error names the field: {msg}");
    }

    /// Webhook `endpoint_url` rendering to a non-URL must fail the guard
    /// (mirrors `send`'s URL parse), naming the announcer.
    #[test]
    fn invalid_rendered_webhook_url_fails_naming_announcer() {
        let announce = AnnounceConfig {
            webhook: Some(WebhookConfig {
                enabled: enabled(),
                // Renders cleanly to "not a url" — `send`'s URL parse rejects it.
                endpoint_url: Some("not a url {{ Tag }}".to_string()),
                message_template: Some("{{ ProjectName }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        let err = validate_announce_templates(&mut ctx, &log)
            .expect_err("invalid rendered endpoint_url must fail the guard");
        let msg = format!("{err:#}");
        assert!(msg.contains("webhook"), "error names the announcer: {msg}");
    }

    /// `announce.skip` true short-circuits the guard.
    #[test]
    fn skip_short_circuits() {
        let announce = AnnounceConfig {
            skip: Some(StringOrBool::Bool(true)),
            discord: Some(DiscordAnnounce {
                enabled: enabled(),
                message_template: Some("{{ NoSuchVar }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(announce, ContextOptions::default());
        let log = ctx.logger("prepublish-guard");
        validate_announce_templates(&mut ctx, &log)
            .expect("announce.skip=true short-circuits the guard");
    }
}
