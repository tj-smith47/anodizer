//! Root `on_error:` hooks for `anodizer release`.
//!
//! A release-pipeline failure — build, sign, package, publish, anything the
//! dispatched mode ran — leaves every tag, commit, and published artifact
//! exactly where it landed. The one thing it fires is the operator's root
//! `on_error:` hook list, so notification / cleanup automation still sees
//! the failure.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

/// `ANODIZER_*` env var → template var pairs exported to a release-level
/// `on_error` hook. Same injection-safety rationale as the per-publisher
/// failure hooks: `{{ .Error }}` carries remote-controlled text (HTTP
/// bodies, subprocess stderr), so hooks read `"$ANODIZER_ERROR"` instead
/// of interpolating it into the command string.
const RELEASE_ON_ERROR_ENV_VARS: [(&str, &str); 4] = [
    ("ANODIZER_ERROR", "Error"),
    ("ANODIZER_ROLLED_BACK", "RolledBack"),
    ("ANODIZER_VERSION", "Version"),
    ("ANODIZER_TAG", "Tag"),
];

/// Fire the root `on_error:` hooks after a release-pipeline failure.
///
/// Notification / cleanup hooks: a hook's own failure is logged and never
/// masks the pipeline error. Dry-run previews the hooks instead of
/// executing them (the standard hook-runner behavior).
///
/// `{{ .RolledBack }}` / `$ANODIZER_ROLLED_BACK` is always `false`: the
/// pipeline no longer withdraws anything on its own. The var stays exported
/// so hook templates written against it keep rendering; deliberate
/// withdrawal is `anodizer tag rollback`, which is a separate command with
/// its own output.
pub(super) fn fire_release_on_error(ctx: &Context, err: &anyhow::Error, log: &StageLogger) {
    let Some(hooks) = ctx
        .config
        .on_error
        .as_ref()
        .and_then(|h| h.hooks.as_deref())
    else {
        return;
    };
    if hooks.is_empty() {
        return;
    }
    let mut vars = ctx.template_vars().clone();
    vars.set("Error", &format!("{err:#}"));
    vars.set("RolledBack", "false");
    let env: Vec<(String, String)> = RELEASE_ON_ERROR_ENV_VARS
        .iter()
        .map(|(env_key, var_key)| {
            let value = vars.get(var_key).cloned().unwrap_or_default();
            ((*env_key).to_string(), value)
        })
        .collect();
    let hook_ctx = anodizer_core::hooks::HookRunContext::new(ctx.is_dry_run(), log, Some(&vars))
        .with_extra_env(&env);
    if let Err(hook_err) = anodizer_core::hooks::run_hooks(hooks, "on-error", hook_ctx) {
        log.warn(&format!(
            "on-error hook failed (ignored — notification/cleanup hooks never mask the \
             pipeline error): {hook_err:#}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::Verbosity;

    /// Root `on_error:` parses identically in all three config modes —
    /// a root-level hook block like `before:` / `after:`, so single-crate,
    /// lockstep, and per-crate configs resolve the same hook list.
    #[test]
    fn on_error_hooks_parse_in_every_config_mode() {
        for (label, yaml) in [
            (
                "single-crate",
                "project_name: app\non_error:\n  hooks:\n    - ./notify.sh\ncrates:\n  - name: app\n    path: \".\"\n",
            ),
            (
                "lockstep",
                "project_name: ws\non_error:\n  hooks:\n    - ./notify.sh\nworkspaces:\n  - name: ws\n    crates:\n      - name: a\n        path: crates/a\n        tag_template: \"v{{ Version }}\"\n",
            ),
            (
                "per-crate",
                "project_name: ws\non_error:\n  hooks:\n    - ./notify.sh\nworkspaces:\n  - name: ws\n    crates:\n      - name: a\n        path: crates/a\n        tag_template: \"a-v{{ Version }}\"\n",
            ),
        ] {
            let config: Config = serde_yaml_ng::from_str(yaml)
                .unwrap_or_else(|e| panic!("{label}: config must parse: {e}"));
            let hooks = config
                .on_error
                .as_ref()
                .and_then(|h| h.hooks.as_deref())
                .unwrap_or_default();
            assert_eq!(hooks.len(), 1, "{label}: one on_error hook resolves");
        }
    }

    /// Root `on_error:` hooks fire on a pipeline failure with the error
    /// text delivered via `ANODIZER_*` env vars (never interpolated into
    /// the command string). `ANODIZER_ROLLED_BACK` is pinned to `false`:
    /// the pipeline withdraws nothing on its own.
    #[test]
    #[cfg(unix)]
    fn release_on_error_hooks_fire_with_error_env() {
        use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let out_str = out.display().to_string();
        let config = Config {
            on_error: Some(HooksConfig {
                hooks: Some(vec![HookEntry::Structured(StructuredHook {
                    cmd: format!(
                        "printf '%s\\n' \"err=$ANODIZER_ERROR rolled=$ANODIZER_ROLLED_BACK\" \
                         >> {out_str}"
                    ),
                    ..Default::default()
                })]),
                post: None,
            }),
            ..Default::default()
        };
        let ctx = Context::new(config, ContextOptions::default());
        let log = StageLogger::new("test", Verbosity::Quiet);
        fire_release_on_error(&ctx, &anyhow::anyhow!("sign stage failed: boom"), &log);
        let fired = std::fs::read_to_string(&out).expect("hook must have fired");
        assert!(
            fired.contains("err=sign stage failed: boom"),
            "hook must see the pipeline error via env: {fired}"
        );
        assert!(
            fired.contains("rolled=false"),
            "the pipeline never auto-withdraws, so the hook must see rolled=false: {fired}"
        );
    }

    /// With no root `on_error:` configured, a pipeline failure fires
    /// nothing and panics nothing.
    #[test]
    fn release_on_error_without_hooks_is_a_noop() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = StageLogger::new("test", Verbosity::Quiet);
        fire_release_on_error(&ctx, &anyhow::anyhow!("boom"), &log);
    }
}
