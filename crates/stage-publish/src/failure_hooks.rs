//! Config-declarable `on_error` / `on_rollback` hook firing.
//!
//! Two firing points, both reusing the standard hook machinery
//! ([`anodizer_core::hooks::run_hooks`] over [`HookEntry`]):
//!
//! - [`fire_on_error`] runs once per FAILED publisher in the dispatch
//!   failure path, BEFORE that publisher is rolled back.
//! - [`fire_on_rollback`] runs once per publisher that is actually rolled
//!   back, in the rollback path.
//!
//! Effective hooks are resolved per publisher from the in-scope crates'
//! `publish:` blocks via [`PublishConfig::effective_on_error`] /
//! [`PublishConfig::effective_on_rollback`]: a per-publisher override
//! REPLACES the publish-wide default (most-specific wins, no double-fire).
//!
//! These are notification / cleanup hooks. A hook's OWN failure must never
//! cascade: it is logged as a warning via [`StageLogger`] and execution
//! continues, so a broken notifier cannot change the release exit status or
//! abort the remaining rollbacks.

use anodizer_core::config::{HookEntry, PublishConfig};
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::log::StageLogger;
use anodizer_core::template::TemplateVars;
use anodizer_core::{PublisherGroup, PublisherResult};

/// Which failure-hook surface to resolve for a publisher.
#[derive(Clone, Copy)]
enum HookKind {
    OnError,
    OnRollback,
}

/// Walk the in-scope crates' `publish:` blocks and return the first crate
/// whose effective hook list (per `kind`) is non-empty for `publisher`.
///
/// Scope follows the active per-crate run: when `selected_crates` is set
/// (workspace per-crate mode), only those crates are consulted; otherwise
/// (single-crate / lockstep) every crate is. The first match wins — in
/// lockstep mode a release fires one publish pipeline, so a single declaring
/// crate is the intended source; per-crate mode runs one pipeline per crate,
/// so each run sees only its own crate's hooks. Returned hooks are cloned so
/// the borrow on `ctx.config` does not outlive the subsequent `run_hooks`
/// call, which needs `&StageLogger` (and, at the rollback site, leaves `ctx`
/// free for other rollback steps).
fn resolve_hooks(ctx: &Context, publisher: &str, kind: HookKind) -> Vec<HookEntry> {
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.iter().any(|s| s == &c.name) {
            continue;
        }
        let Some(publish) = c.publish.as_ref() else {
            continue;
        };
        if let Some(hooks) = effective(publish, publisher, kind)
            && !hooks.is_empty()
        {
            return hooks.to_vec();
        }
    }
    Vec::new()
}

fn effective<'a>(
    publish: &'a PublishConfig,
    publisher: &str,
    kind: HookKind,
) -> Option<&'a [HookEntry]> {
    match kind {
        HookKind::OnError => publish.effective_on_error(publisher),
        HookKind::OnRollback => publish.effective_on_rollback(publisher),
    }
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
/// `{{ .Required }}`. `{{ .Version }}` / `{{ .Tag }}` are already bound on
/// `ctx.template_vars()` for the current crate scope, so they resolve
/// per-crate correctly in every config mode without re-binding here.
fn bind_failure_vars(base: &TemplateVars, result: &PublisherResult, error: &str) -> TemplateVars {
    let mut vars = base.clone();
    vars.set("Publisher", &result.name);
    vars.set("Error", error);
    vars.set("Group", group_label(result.group));
    vars.set("Required", if result.required { "true" } else { "false" });
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
/// dispatch failure path BEFORE the publisher is rolled back. `error` is the
/// publisher's failure message (the `{{ .Error }}` value).
pub(crate) fn fire_on_error(
    ctx: &Context,
    result: &PublisherResult,
    error: &str,
    log: &StageLogger,
) {
    let hooks = resolve_hooks(ctx, &result.name, HookKind::OnError);
    if hooks.is_empty() {
        return;
    }
    let vars = bind_failure_vars(ctx.template_vars(), result, error);
    run_warn_only(&hooks, "on-error", ctx.is_dry_run(), log, &vars);
}

/// Fire `on_rollback` hooks for a single publisher whose rollback step ran.
/// Called from the rollback path once the rollback action has been attempted
/// — for BOTH a successful revert (`RolledBack`) and a failed one
/// (`RollbackFailed`), since a failed rollback (a live artifact that could
/// not be pulled) is the MORE important signal for the operator's
/// notification/cleanup hook. Rows that were never attempted (no-scope,
/// publisher-not-in-registry) do not reach this call. `error` carries the
/// originating failure message when the rolled-back row was a `Failed`
/// submitter (cargo); for a reverted `Succeeded` Assets/Manager publisher it
/// is empty.
pub(crate) fn fire_on_rollback(
    ctx: &Context,
    result: &PublisherResult,
    error: &str,
    log: &StageLogger,
) {
    let hooks = resolve_hooks(ctx, &result.name, HookKind::OnRollback);
    if hooks.is_empty() {
        return;
    }
    let vars = bind_failure_vars(ctx.template_vars(), result, error);
    run_warn_only(&hooks, "on-rollback", ctx.is_dry_run(), log, &vars);
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, PublishConfig, StructuredHook};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult};
    use std::collections::BTreeMap;
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
                "P={{ .Publisher }} E={{ .Error }} V={{ .Version }} G={{ .Group }} R={{ .Required }}",
            )]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.2.3")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(&ctx, &res, "tap push rejected", &log());

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "P=homebrew E=tap push rejected V=1.2.3 G=Manager R=true",
            "on_error must bind .Publisher/.Error/.Version/.Group/.Required"
        );
    }

    #[test]
    fn on_rollback_fires_for_rolled_back_publisher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_rollback: Some(vec![probe_hook(&out, "rb {{ .Publisher }} {{ .Tag }}")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v9.9.9")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let mut res = result("github-release", PublisherGroup::Assets, true);
        res.outcome = PublisherOutcome::RolledBack;

        fire_on_rollback(&ctx, &res, "", &log());

        let body = std::fs::read_to_string(&out).expect("on_rollback hook must have run");
        assert_eq!(body.trim(), "rb github-release v9.9.9");
    }

    #[test]
    fn per_publisher_override_replaces_default_no_double_fire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "homebrew".to_string(),
            vec![probe_hook(&out, "override {{ .Publisher }}")],
        );
        let publish = PublishConfig {
            // The default would also write — if it fired we'd see two lines.
            on_error: Some(vec![probe_hook(&out, "default {{ .Publisher }}")]),
            on_error_per_publisher: Some(overrides),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(&ctx, &res, "boom", &log());

        let body = std::fs::read_to_string(&out).expect("override hook must run");
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines,
            vec!["override homebrew"],
            "per-publisher override must REPLACE the default (no double-fire)"
        );
    }

    #[test]
    fn default_applies_when_no_override_for_publisher() {
        // A per-publisher override for a DIFFERENT publisher must not
        // suppress the top-level default for this one.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let mut overrides = BTreeMap::new();
        overrides.insert("scoop".to_string(), vec![cmd_hook("true")]);
        let publish = PublishConfig {
            on_error: Some(vec![probe_hook(&out, "default {{ .Publisher }}")]),
            on_error_per_publisher: Some(overrides),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(&ctx, &res, "boom", &log());

        let body = std::fs::read_to_string(&out).expect("default hook must run");
        assert_eq!(body.trim(), "default homebrew");
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
        fire_on_error(&ctx, &res, "boom", &log());
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

        fire_on_error(&ctx, &res, "boom", &log());

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

        fire_on_error(&ctx, &res, "boom", &log());

        let body = std::fs::read_to_string(&out).expect("scoped crate hook must run");
        assert_eq!(
            body.trim(),
            "cargo@2.0.0",
            "only the in-scope crate's hooks fire; .Version resolves per crate"
        );
    }
}
