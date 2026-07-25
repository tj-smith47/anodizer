//! Root `always:` hooks for `anodizer release` — the pipeline's `finally`.
//!
//! Every terminal path of a release run funnels through here: a success
//! that already fired `after:`, a pipeline failure that already fired
//! `on_error:`, and the exit neither of those reaches — a `before:` hook
//! that failed before the pipeline started. Firing at the single top-level
//! exit of the command (rather than inside the post-pipeline tail, where
//! `after:` lives) is what makes that coverage total: the `--split` build
//! leg never reaches the post-pipeline tail at all, so teardown wired
//! there would silently never run on a shard.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

/// Fire the root `always:` hooks and resolve the run's final result.
///
/// `outcome` is whatever the rest of the command produced. The hooks see it
/// through `{{ .Success }}` / `{{ .Error }}` and the matching `ANODIZER_*`
/// env vars, then:
///
/// - failure path — a hook's own failure is logged as a warning and
///   `outcome`'s original error is returned unchanged, so the operator
///   still sees what actually broke the release;
/// - success path — there is no error to mask, so a hook failure becomes
///   the run's error, matching `after:`.
///
/// Dry-run previews the hooks instead of executing them (the standard
/// hook-runner behavior).
pub(super) fn finish_with_always_hooks(
    ctx: &Context,
    outcome: Result<()>,
    log: &StageLogger,
) -> Result<()> {
    let Some(hooks) = ctx.config.always.as_ref().and_then(|h| h.hooks.as_deref()) else {
        return outcome;
    };
    if hooks.is_empty() {
        return outcome;
    }

    let success = outcome.is_ok();
    let error_text = outcome
        .as_ref()
        .err()
        .map(|err| format!("{err:#}"))
        .unwrap_or_default();

    let mut vars = ctx.template_vars().clone();
    // A real `Value::Bool` so `{% if Success %}` branches instead of always
    // taking the truthy arm on the string "false".
    vars.set_bool("Success", success);
    vars.set("Error", &error_text);
    // Built from the local values rather than read back out of `vars`:
    // `set_bool` writes the structured map, which `TemplateVars::get` does
    // not see, so a get-based export would ship an empty ANODIZER_SUCCESS.
    //
    // Same injection-safety rationale as the per-publisher failure hooks:
    // `{{ .Error }}` carries remote-controlled text (HTTP bodies,
    // subprocess stderr), so hooks read `"$ANODIZER_ERROR"` instead of
    // interpolating it into the command string.
    let env: Vec<(String, String)> = vec![
        (
            "ANODIZER_SUCCESS".to_string(),
            if success { "true" } else { "false" }.to_string(),
        ),
        ("ANODIZER_ERROR".to_string(), error_text),
        (
            "ANODIZER_VERSION".to_string(),
            vars.get("Version").cloned().unwrap_or_default(),
        ),
        (
            "ANODIZER_TAG".to_string(),
            vars.get("Tag").cloned().unwrap_or_default(),
        ),
    ];

    let hook_ctx = anodizer_core::hooks::HookRunContext::new(ctx.is_dry_run(), log, Some(&vars))
        .with_extra_env(&env);
    match anodizer_core::hooks::run_hooks(hooks, "always", hook_ctx) {
        Ok(()) => outcome,
        Err(hook_err) => match outcome {
            Err(original) => {
                log.warn(&format!(
                    "always hook failed (ignored — a teardown hook never masks the \
                     release failure it is cleaning up after): {hook_err:#}"
                ));
                Err(original)
            }
            Ok(()) => Err(hook_err),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{Config, HookEntry, HooksConfig, StructuredHook};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::Verbosity;

    /// Root `always:` parses identically in all three config modes — a
    /// root-level hook block like `before:` / `after:` / `on_error:`, so
    /// single-crate, lockstep, and per-crate configs resolve the same hook
    /// list.
    #[test]
    fn always_hooks_parse_in_every_config_mode() {
        for (label, yaml) in [
            (
                "single-crate",
                "project_name: app\nalways:\n  hooks:\n    - ./teardown.sh\ncrates:\n  - name: app\n    path: \".\"\n",
            ),
            (
                "lockstep",
                "project_name: ws\nalways:\n  hooks:\n    - ./teardown.sh\nworkspaces:\n  - name: ws\n    crates:\n      - name: a\n        path: crates/a\n        tag_template: \"v{{ Version }}\"\n",
            ),
            (
                "per-crate",
                "project_name: ws\nalways:\n  hooks:\n    - ./teardown.sh\nworkspaces:\n  - name: ws\n    crates:\n      - name: a\n        path: crates/a\n        tag_template: \"a-v{{ Version }}\"\n",
            ),
        ] {
            let config: Config = serde_yaml_ng::from_str(yaml)
                .unwrap_or_else(|e| panic!("{label}: config must parse: {e}"));
            let hooks = config
                .always
                .as_ref()
                .and_then(|h| h.hooks.as_deref())
                .unwrap_or_default();
            assert_eq!(hooks.len(), 1, "{label}: one always hook resolves");
        }
    }

    /// A `Context` whose root `always:` block runs `cmd`.
    #[cfg(unix)]
    fn ctx_with_always(cmd: String) -> Context {
        let config = Config {
            always: Some(HooksConfig {
                hooks: Some(vec![HookEntry::Structured(StructuredHook {
                    cmd,
                    ..Default::default()
                })]),
                post: None,
            }),
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    /// On the success path the hooks see `Success=true` and an empty
    /// `ANODIZER_ERROR`.
    #[test]
    #[cfg(unix)]
    fn always_hooks_fire_on_success_with_outcome_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let ctx = ctx_with_always(format!(
            "printf '%s\\n' \"success=$ANODIZER_SUCCESS err=[$ANODIZER_ERROR]\" >> {}",
            out.display()
        ));
        let log = StageLogger::new("test", Verbosity::Quiet);
        finish_with_always_hooks(&ctx, Ok(()), &log).expect("success path must stay Ok");
        let fired = std::fs::read_to_string(&out).expect("hook must have fired");
        assert!(
            fired.contains("success=true") && fired.contains("err=[]"),
            "success run must export success=true and an empty error: {fired}"
        );
    }

    /// On the failure path the hooks see `Success=false` plus the error
    /// text, and the original error is what the run returns.
    #[test]
    #[cfg(unix)]
    fn always_hooks_fire_on_failure_and_preserve_the_original_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let ctx = ctx_with_always(format!(
            "printf '%s\\n' \"success=$ANODIZER_SUCCESS err=$ANODIZER_ERROR\" >> {}",
            out.display()
        ));
        let log = StageLogger::new("test", Verbosity::Quiet);
        let err =
            finish_with_always_hooks(&ctx, Err(anyhow::anyhow!("sign stage failed: boom")), &log)
                .expect_err("the pipeline error must survive");
        assert!(
            format!("{err:#}").contains("sign stage failed: boom"),
            "original error must be returned verbatim: {err:#}"
        );
        let fired = std::fs::read_to_string(&out).expect("hook must have fired");
        assert!(
            fired.contains("success=false") && fired.contains("err=sign stage failed: boom"),
            "failed run must export success=false and the error text: {fired}"
        );
    }

    /// A failing `always:` hook on the failure path is a warning, never a
    /// replacement for the pipeline error the operator needs to see.
    #[test]
    #[cfg(unix)]
    fn failing_always_hook_never_masks_the_pipeline_error() {
        let ctx = ctx_with_always("exit 3".to_string());
        let log = StageLogger::new("test", Verbosity::Quiet);
        let err = finish_with_always_hooks(&ctx, Err(anyhow::anyhow!("publish failed: 502")), &log)
            .expect_err("failure path must stay Err");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("publish failed: 502"),
            "the pipeline error must be the returned one: {rendered}"
        );
    }

    /// A failing `always:` hook on the success path has no error to mask,
    /// so it fails the run — the same contract `after:` has.
    #[test]
    #[cfg(unix)]
    fn failing_always_hook_fails_an_otherwise_successful_run() {
        let ctx = ctx_with_always("exit 3".to_string());
        let log = StageLogger::new("test", Verbosity::Quiet);
        finish_with_always_hooks(&ctx, Ok(()), &log)
            .expect_err("a failing teardown must fail a successful run");
    }

    /// With no root `always:` configured the outcome passes through
    /// untouched on both paths.
    #[test]
    fn always_without_hooks_passes_the_outcome_through() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = StageLogger::new("test", Verbosity::Quiet);
        finish_with_always_hooks(&ctx, Ok(()), &log).expect("Ok passes through");
        let err = finish_with_always_hooks(&ctx, Err(anyhow::anyhow!("boom")), &log)
            .expect_err("Err passes through");
        assert!(format!("{err:#}").contains("boom"));
    }
}
