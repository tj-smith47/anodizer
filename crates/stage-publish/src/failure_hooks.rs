//! Config-declarable `on_error` hook firing.
//!
//! One firing point, reusing the standard hook machinery
//! ([`anodizer_core::hooks::run_hooks`] over [`HookEntry`]):
//!
//! - [`fire_on_error`] runs once per FAILED publisher in the dispatch
//!   failure path, AFTER rollback has been attempted. The `rolled_back`
//!   flag indicates whether the publisher was successfully reverted and is
//!   exposed as `{{ .RolledBack }}` in the hook template surface.
//!
//! Effective hooks are resolved per publisher from the in-scope crates'
//! `publish:` blocks via `publish.on_error`.
//!
//! These are notification / cleanup hooks. A hook's OWN failure must never
//! cascade: it is logged as a warning via [`StageLogger`] and execution
//! continues, so a broken notifier cannot change the release exit status or
//! abort the remaining rollbacks.

use anodizer_core::config::HookEntry;
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::log::StageLogger;
use anodizer_core::template::TemplateVars;
use anodizer_core::{PublisherGroup, PublisherResult};

/// Walk the in-scope crates' `publish:` blocks and return the `on_error`
/// hooks for the first crate that declares them.
///
/// Scope follows the active per-crate run: when `selected_crates` is set
/// (workspace per-crate mode), only those crates are consulted; otherwise
/// (single-crate / lockstep) every crate is. Returned hooks are cloned so
/// the borrow on `ctx.config` does not outlive the subsequent `run_hooks`
/// call.
fn resolve_on_error_hooks(ctx: &Context) -> Vec<HookEntry> {
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.iter().any(|s| s == &c.name) {
            continue;
        }
        let Some(publish) = c.publish.as_ref() else {
            continue;
        };
        if let Some(hooks) = publish.on_error.as_deref() {
            return hooks.to_vec();
        }
    }
    Vec::new()
}

/// Map a [`PublisherGroup`] to the `{{ .Group }}` template value.
fn group_label(group: PublisherGroup) -> &'static str {
    match group {
        PublisherGroup::Assets => "Assets",
        PublisherGroup::Manager => "Manager",
        PublisherGroup::Submitter => "Submitter",
    }
}

/// Bind the failure-hook template surface on top of the standard per-crate
/// vars: `{{ .Publisher }}`, `{{ .Error }}`, `{{ .Group }}`,
/// `{{ .Required }}`, `{{ .RolledBack }}`. `{{ .Version }}` / `{{ .Tag }}`
/// are already bound on `ctx.template_vars()` for the current crate scope,
/// so they resolve per-crate correctly in every config mode without
/// re-binding here.
fn bind_failure_vars(
    base: &TemplateVars,
    result: &PublisherResult,
    error: &str,
    rolled_back: bool,
) -> TemplateVars {
    let mut vars = base.clone();
    vars.set("Publisher", &result.name);
    vars.set("Error", error);
    vars.set("Group", group_label(result.group));
    vars.set("Required", if result.required { "true" } else { "false" });
    vars.set("RolledBack", if rolled_back { "true" } else { "false" });
    vars
}

/// Execute the resolved hook list, downgrading any hook failure to a warning
/// so it cannot cascade into the release outcome or abort sibling steps.
fn run_warn_only(
    hooks: &[HookEntry],
    label: &str,
    dry_run: bool,
    log: &StageLogger,
    vars: &TemplateVars,
) {
    let ctx = HookRunContext::new(dry_run, log, Some(vars));
    if let Err(err) = run_hooks(hooks, label, ctx) {
        log.warn(&format!(
            "{label} hook failed (ignored — notification/cleanup hooks never fail the release): {err:#}"
        ));
    }
}

/// Fire `on_error` hooks for a single FAILED publisher. Called from the
/// dispatch failure path AFTER rollback has been attempted. `error` is the
/// publisher's failure message (the `{{ .Error }}` value). `rolled_back`
/// indicates whether the publisher was successfully reverted and is exposed
/// as `{{ .RolledBack }}` in the template surface.
pub(crate) fn fire_on_error(
    ctx: &Context,
    result: &PublisherResult,
    error: &str,
    rolled_back: bool,
    log: &StageLogger,
) {
    let hooks = resolve_on_error_hooks(ctx);
    if hooks.is_empty() {
        return;
    }
    let vars = bind_failure_vars(ctx.template_vars(), result, error, rolled_back);
    run_warn_only(&hooks, "on-error", ctx.is_dry_run(), log, &vars);
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, PublishConfig, StructuredHook};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult};
    use std::path::Path;

    fn log() -> StageLogger {
        StageLogger::new("test", Verbosity::Normal)
    }

    /// A structured hook that appends a rendered line to `out` so the test
    /// can assert which template vars the hook actually observed.
    fn probe_hook(out: &Path, body: &str) -> HookEntry {
        let out = out.display().to_string().replace('\\', "/");
        HookEntry::Structured(StructuredHook {
            cmd: format!("printf '%s\\n' '{body}' >> {out}"),
            ..Default::default()
        })
    }

    fn cmd_hook(cmd: &str) -> HookEntry {
        HookEntry::Structured(StructuredHook {
            cmd: cmd.to_string(),
            ..Default::default()
        })
    }

    fn crate_with_publish(name: &str, publish: PublishConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            publish: Some(publish),
            ..Default::default()
        }
    }

    fn result(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        }
    }

    #[test]
    fn on_error_fires_with_publisher_error_and_version_vars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_error: Some(vec![probe_hook(
                &out,
                "P={{ .Publisher }} E={{ .Error }} V={{ .Version }} G={{ .Group }} R={{ .Required }} RB={{ .RolledBack }}",
            )]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.2.3")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(&ctx, &res, "tap push rejected", true, &log());

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "P=homebrew E=tap push rejected V=1.2.3 G=Manager R=true RB=true",
            "on_error must bind .Publisher/.Error/.Version/.Group/.Required/.RolledBack"
        );
    }

    #[test]
    fn rolled_back_false_when_not_reverted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_error: Some(vec![probe_hook(&out, "RB={{ .RolledBack }}")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("cargo", PublisherGroup::Submitter, true);

        fire_on_error(&ctx, &res, "publish failed", false, &log());

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "RB=false",
            ".RolledBack must be false when rolled_back=false"
        );
    }

    #[test]
    fn failing_hook_warns_but_does_not_panic_or_cascade() {
        // A hook whose command exits non-zero must be swallowed: the call
        // returns normally so the surrounding release outcome / rollback
        // loop is untouched.
        let publish = PublishConfig {
            on_error: Some(vec![cmd_hook("exit 7")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("cargo", PublisherGroup::Submitter, true);

        // Must not panic / propagate — the function has no Result to inspect;
        // reaching the assert proves it returned after warn-only handling.
        fire_on_error(&ctx, &res, "boom", false, &log());
    }

    #[test]
    fn dry_run_logs_but_does_not_execute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_error: Some(vec![probe_hook(&out, "should-not-run")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .dry_run(true)
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(&ctx, &res, "boom", false, &log());

        assert!(
            !out.exists(),
            "dry-run must log the hook without executing it"
        );
    }

    #[test]
    fn per_crate_mode_resolves_version_per_crate() {
        // Per-crate mode: dispatch runs scoped to one crate. The hook's
        // `.Version` resolves from the per-crate template vars bound for
        // that run — here `v2.0.0` — and only the in-scope crate's hooks
        // fire.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let scoped = PublishConfig {
            on_error: Some(vec![probe_hook(&out, "{{ .Publisher }}@{{ .Version }}")]),
            ..Default::default()
        };
        let other = PublishConfig {
            on_error: Some(vec![probe_hook(&out, "WRONG-CRATE")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v2.0.0")
            .crates(vec![
                crate_with_publish("app-core", scoped),
                crate_with_publish("app-cli", other),
            ])
            .selected_crates(vec!["app-core".to_string()])
            .build();
        let res = result("cargo", PublisherGroup::Submitter, true);

        fire_on_error(&ctx, &res, "boom", false, &log());

        let body = std::fs::read_to_string(&out).expect("scoped crate hook must run");
        assert_eq!(
            body.trim(),
            "cargo@2.0.0",
            "only the in-scope crate's hooks fire; .Version resolves per crate"
        );
    }
}
