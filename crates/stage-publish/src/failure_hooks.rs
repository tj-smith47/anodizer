//! Config-declarable `on_error` hook firing.
//!
//! One firing point, reusing the standard hook machinery
//! ([`anodizer_core::hooks::run_hooks`] over [`HookEntry`]):
//!
//! - [`fire_on_error`] runs once per FAILED publisher in the dispatch
//!   failure path, AFTER rollback has been attempted. The
//!   `rollback_happened` flag is RUN-WIDE — true if any publisher was
//!   rolled back (or rollback was attempted and failed) during this run —
//!   and is exposed as `{{ .RolledBack }}` in the hook template surface.
//!
//! Effective hooks are resolved per publisher from the in-scope crates'
//! `publish:` blocks via `publish.on_error`.
//!
//! Every template var is ALSO exported to the hook process as an
//! `ANODIZER_*` environment variable (see [`FAILURE_ENV_VARS`]) so hooks can
//! consume untrusted values (`.Error` carries remote-controlled text) without
//! interpolating them into the shell command string.
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

/// Walk the in-scope crates' `publish:` blocks and return the failure hooks
/// (`select` picks the `on_error` / `on_rollback` list) for the first crate
/// that declares them.
///
/// Scope follows the active per-crate run: when `selected_crates` is set
/// (workspace per-crate mode), only those crates are consulted; otherwise
/// (single-crate / lockstep) every crate is. Returned hooks are cloned so
/// the borrow on `ctx.config` does not outlive the subsequent `run_hooks`
/// call.
fn resolve_hooks(
    ctx: &Context,
    select: impl Fn(&anodizer_core::config::PublishConfig) -> Option<&[HookEntry]>,
) -> Vec<HookEntry> {
    let selected = &ctx.options.selected_crates;
    for c in ctx.config.crate_universe() {
        if !selected.is_empty() && !selected.iter().any(|s| s == &c.name) {
            continue;
        }
        let Some(publish) = c.publish.as_ref() else {
            continue;
        };
        if let Some(hooks) = select(publish) {
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

/// The failure-context key/value pairs bound on top of the standard
/// per-crate vars — the single source of truth for the template binding
/// ([`bind_failure_vars`]). Every key here must have a matching
/// [`FAILURE_ENV_VARS`] entry so the env channel never silently lags the
/// template surface; the exhaustiveness test pins that invariant.
/// `Version` / `Tag` are deliberately absent: they are already bound on
/// `ctx.template_vars()` for the current crate scope, so they resolve
/// per-crate correctly in every config mode without re-binding here.
fn failure_var_values(
    result: &PublisherResult,
    error: &str,
    rollback_happened: bool,
    run_report: &str,
) -> [(&'static str, String); 6] {
    let bool_str = |b: bool| (if b { "true" } else { "false" }).to_string();
    [
        ("Publisher", result.name.clone()),
        ("Error", error.to_string()),
        ("Group", group_label(result.group).to_string()),
        ("Required", bool_str(result.required)),
        ("RolledBack", bool_str(rollback_happened)),
        // `dist/run-<id>/report.json` for THIS run — already written when
        // the hook fires; empty when the report was not persisted
        // (snapshot / dry-run / write failure) so hooks can gate on it.
        ("RunReport", run_report.to_string()),
    ]
}

/// Bind the failure-hook template surface ([`failure_var_values`]) on top
/// of the standard per-crate vars: `{{ .Publisher }}`, `{{ .Error }}`,
/// `{{ .Group }}`, `{{ .Required }}`, `{{ .RolledBack }}`, `{{ .RunReport }}`.
fn bind_failure_vars(
    base: &TemplateVars,
    result: &PublisherResult,
    error: &str,
    rollback_happened: bool,
    run_report: &str,
) -> TemplateVars {
    let mut vars = base.clone();
    for (key, value) in failure_var_values(result, error, rollback_happened, run_report) {
        vars.set(key, &value);
    }
    vars
}

/// `ANODIZER_*` env var → template var pairs exported to the hook process.
/// The env channel carries the SAME values as the template surface but
/// reaches the hook without passing through `sh -c` parsing — `.Error`
/// holds remote-controlled text (HTTP error bodies, git stderr), so
/// interpolating it into the command string is a shell-injection vector,
/// while an env var read (`"$ANODIZER_ERROR"`) is not.
const FAILURE_ENV_VARS: [(&str, &str); 8] = [
    ("ANODIZER_PUBLISHER", "Publisher"),
    ("ANODIZER_ERROR", "Error"),
    ("ANODIZER_VERSION", "Version"),
    ("ANODIZER_TAG", "Tag"),
    ("ANODIZER_GROUP", "Group"),
    ("ANODIZER_REQUIRED", "Required"),
    ("ANODIZER_ROLLED_BACK", "RolledBack"),
    ("ANODIZER_RUN_REPORT", "RunReport"),
];

/// Project the bound vars into `ANODIZER_*` env pairs via `env_table`.
/// Reading back from `vars` (rather than re-deriving from the publisher
/// result) keeps the env channel equal to the template surface by
/// construction — including the per-crate-scoped `Version` / `Tag` already
/// bound on the context.
fn project_env(vars: &TemplateVars, env_table: &[(&str, &str)]) -> Vec<(String, String)> {
    env_table
        .iter()
        .map(|(env_key, var_key)| {
            let value = vars.get(var_key).cloned().unwrap_or_default();
            ((*env_key).to_string(), value)
        })
        .collect()
}

/// Execute the resolved hook list, downgrading any hook failure to a warning
/// so it cannot cascade into the release outcome or abort sibling steps.
/// `env_table` selects which vars are also exported as `ANODIZER_*` process
/// environment (the failure-hook or rollback-hook table).
fn run_warn_only(
    hooks: &[HookEntry],
    label: &str,
    dry_run: bool,
    log: &StageLogger,
    vars: &TemplateVars,
    env_table: &[(&str, &str)],
) {
    let env = project_env(vars, env_table);
    let ctx = HookRunContext::new(dry_run, log, Some(vars)).with_extra_env(&env);
    if let Err(err) = run_hooks(hooks, label, ctx) {
        log.warn(&format!(
            "{label} hook failed (ignored — notification/cleanup hooks never fail the release): {err:#}"
        ));
    }
}

/// Derive the `{{ .RunReport }}` / `ANODIZER_RUN_REPORT` value: the persisted
/// run-report path, or empty when none exists (never a stale pointer).
pub(crate) fn run_report_var(ctx: &Context) -> String {
    crate::existing_run_report_path(ctx)
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Fire `on_error` hooks for a single FAILED publisher. Called from the
/// dispatch failure path AFTER rollback has been attempted. `error` is the
/// publisher's failure message (the `{{ .Error }}` value).
/// `rollback_happened` is RUN-WIDE — true if any publisher was rolled back
/// (or rollback was attempted and failed) during this run — and is exposed
/// as `{{ .RolledBack }}` in the template surface.
/// `run_report` is the `{{ .RunReport }}` value — the persisted run-report
/// path, or empty when none exists. It is invariant across a run's fan-out,
/// so the caller derives it once ([`run_report_var`]) and threads it down.
pub(crate) fn fire_on_error(
    ctx: &Context,
    result: &PublisherResult,
    error: &str,
    rollback_happened: bool,
    run_report: &str,
    log: &StageLogger,
) {
    let hooks = resolve_hooks(ctx, |p| p.on_error.as_deref());
    if hooks.is_empty() {
        return;
    }
    let vars = bind_failure_vars(
        ctx.template_vars(),
        result,
        error,
        rollback_happened,
        run_report,
    );
    run_warn_only(
        &hooks,
        "on-error",
        ctx.is_dry_run(),
        log,
        &vars,
        &FAILURE_ENV_VARS,
    );
}

/// The rollback-context key/value pairs bound on top of the standard per-crate
/// vars — the single source of truth for the on_rollback template binding
/// ([`bind_rollback_vars`]). Every key here must have a matching
/// [`ROLLBACK_ENV_VARS`] entry so the env channel never silently lags the
/// template surface; the exhaustiveness test pins that invariant. `Version` /
/// `Tag` are deliberately absent (already bound per-crate on
/// `ctx.template_vars()`), mirroring [`failure_var_values`].
fn rollback_var_values(
    name: &str,
    group: PublisherGroup,
    required: bool,
    rollback_failed: bool,
    error: &str,
    reason: &str,
) -> [(&'static str, String); 6] {
    let bool_str = |b: bool| (if b { "true" } else { "false" }).to_string();
    [
        ("Publisher", name.to_string()),
        ("Group", group_label(group).to_string()),
        ("Required", bool_str(required)),
        ("RollbackFailed", bool_str(rollback_failed)),
        ("Error", error.to_string()),
        // The run-wide sibling failure(s) that triggered the unwind — distinct
        // from `Error` (this publisher's own revert failure). Empty on the
        // `--rollback-only` replay path, which has no live trigger in scope.
        ("Reason", reason.to_string()),
    ]
}

/// Bind the on_rollback template surface ([`rollback_var_values`]) on top of
/// the standard per-crate vars: `{{ .Publisher }}`, `{{ .Group }}`,
/// `{{ .Required }}`, `{{ .RollbackFailed }}`, `{{ .Error }}`, `{{ .Reason }}`.
fn bind_rollback_vars(
    base: &TemplateVars,
    name: &str,
    group: PublisherGroup,
    required: bool,
    rollback_failed: bool,
    error: &str,
    reason: &str,
) -> TemplateVars {
    let mut vars = base.clone();
    for (key, value) in rollback_var_values(name, group, required, rollback_failed, error, reason) {
        vars.set(key, &value);
    }
    vars
}

/// `ANODIZER_*` env var → template var pairs exported to an on_rollback hook.
/// Same construction and injection-safety rationale as [`FAILURE_ENV_VARS`]:
/// `.Error` carries the rollback failure message (git stderr / API body), so
/// hooks read `"$ANODIZER_ERROR"` rather than interpolating it into `cmd`.
const ROLLBACK_ENV_VARS: [(&str, &str); 8] = [
    ("ANODIZER_PUBLISHER", "Publisher"),
    ("ANODIZER_VERSION", "Version"),
    ("ANODIZER_TAG", "Tag"),
    ("ANODIZER_GROUP", "Group"),
    ("ANODIZER_REQUIRED", "Required"),
    ("ANODIZER_ROLLBACK_FAILED", "RollbackFailed"),
    ("ANODIZER_ERROR", "Error"),
    ("ANODIZER_ROLLBACK_REASON", "Reason"),
];

/// Fire `on_rollback` hooks for a single publisher a triggered rollback
/// reverted, called from the rollback path once the publisher's rollback step
/// is final. This fires for a publisher that `Succeeded` and was reverted only
/// because a sibling required publisher failed — the case [`fire_on_error`],
/// which fires solely for the failed publisher, cannot reach. It also fires
/// when the revert itself failed: `rollback_failed` is then `true` (exposed as
/// `{{ .RollbackFailed }}`) and `error` carries the rollback failure message
/// (`{{ .Error }}`, empty on a clean revert). `reason` is the run-wide
/// triggering cause — the sibling required failure(s) that unwound the run —
/// exposed as `{{ .Reason }}`; distinct from `error`, and empty on the
/// `--rollback-only` replay path (no live trigger in scope). Independent of
/// `on_error`: a publisher that both failed and was rolled back fires both
/// hooks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_on_rollback(
    ctx: &Context,
    name: &str,
    group: PublisherGroup,
    required: bool,
    rollback_failed: bool,
    error: &str,
    reason: &str,
    log: &StageLogger,
) {
    let hooks = resolve_hooks(ctx, |p| p.on_rollback.as_deref());
    if hooks.is_empty() {
        return;
    }
    let vars = bind_rollback_vars(
        ctx.template_vars(),
        name,
        group,
        required,
        rollback_failed,
        error,
        reason,
    );
    run_warn_only(
        &hooks,
        "on-rollback",
        ctx.is_dry_run(),
        log,
        &vars,
        &ROLLBACK_ENV_VARS,
    );
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

    /// A hook that dumps `KEY=$KEY` lines for each requested env var so the
    /// test can assert the hook's process environment. The command contains
    /// no template syntax: values must arrive via the environment, never via
    /// string interpolation into the shell command.
    fn env_probe_hook(out: &Path, keys: &[&str]) -> HookEntry {
        let out = out.display().to_string().replace('\\', "/");
        let cmd = keys
            .iter()
            .map(|k| format!("printf '%s\\n' \"{k}=${k}\" >> {out}"))
            .collect::<Vec<_>>()
            .join("; ");
        HookEntry::Structured(StructuredHook {
            cmd,
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

    /// A workspace-only crate's `publish.on_error` hooks must fire: a
    /// `config.crates`-only walk resolved no hooks for a pure-workspace
    /// config, so a failed publish never ran the operator's hook.
    #[test]
    fn on_error_fires_from_workspace_only_crate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let out_str = out.display().to_string().replace('\\', "/");
        let publish = PublishConfig {
            on_error: Some(vec![cmd_hook(&format!("printf 'ws-hook\\n' >> {out_str}"))]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .workspaces(vec![anodizer_core::config::WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![crate_with_publish("ws-only", publish)],
                ..Default::default()
            }])
            .build();
        assert!(
            ctx.config.crates.is_empty(),
            "fixture must be a pure-workspace config"
        );
        let res = result("homebrew", PublisherGroup::Manager, true);
        fire_on_error(
            &ctx,
            &res,
            "tap push rejected",
            false,
            &run_report_var(&ctx),
            &log(),
        );
        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(body, "ws-hook\n");
    }

    /// Lockstep workspace (multiple crates, no per-crate scoping):
    /// `defaults.publish.on_error` is append-merged by `apply_defaults`
    /// into every crate, and the firing path runs the resolved per-crate
    /// list — the crate's own hook first, the defaults hook after it.
    #[test]
    fn on_error_lockstep_defaults_append_merge_fires_both_hooks_in_order() {
        use anodizer_core::config::{Config, Defaults, PublishDefaults};
        use anodizer_core::defaults_merge::apply_defaults;

        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let out_str = out.display().to_string().replace('\\', "/");

        let crate_publish = PublishConfig {
            on_error: Some(vec![cmd_hook(&format!(
                "printf 'crate-hook\\n' >> {out_str}"
            ))]),
            ..Default::default()
        };
        let mut config = Config {
            crates: vec![
                crate_with_publish("a", crate_publish),
                crate_with_publish("b", PublishConfig::default()),
            ],
            defaults: Some(Defaults {
                publish: Some(PublishDefaults {
                    on_error: Some(vec![cmd_hook(&format!(
                        "printf 'default-hook\\n' >> {out_str}"
                    ))]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);

        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(config.crates.clone())
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);
        fire_on_error(
            &ctx,
            &res,
            "tap push rejected",
            false,
            &run_report_var(&ctx),
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hooks must have written output");
        assert_eq!(
            body, "crate-hook\ndefault-hook\n",
            "per-crate hook fires first, append-merged defaults hook after"
        );
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

        fire_on_error(
            &ctx,
            &res,
            "tap push rejected",
            true,
            &run_report_var(&ctx),
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "P=homebrew E=tap push rejected V=1.2.3 G=Manager R=true RB=true",
            "on_error must bind .Publisher/.Error/.Version/.Group/.Required/.RolledBack"
        );
    }

    #[test]
    fn rolled_back_false_when_no_rollback_happened() {
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

        fire_on_error(
            &ctx,
            &res,
            "publish failed",
            false,
            &run_report_var(&ctx),
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "RB=false",
            ".RolledBack must be false when rollback_happened=false"
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
        fire_on_error(&ctx, &res, "boom", false, &run_report_var(&ctx), &log());
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

        fire_on_error(&ctx, &res, "boom", false, &run_report_var(&ctx), &log());

        assert!(
            !out.exists(),
            "dry-run must log the hook without executing it"
        );
    }

    const ALL_ENV_KEYS: [&str; 8] = [
        "ANODIZER_PUBLISHER",
        "ANODIZER_ERROR",
        "ANODIZER_VERSION",
        "ANODIZER_TAG",
        "ANODIZER_GROUP",
        "ANODIZER_REQUIRED",
        "ANODIZER_ROLLED_BACK",
        "ANODIZER_RUN_REPORT",
    ];

    #[test]
    fn env_channel_mirrors_failure_var_surface_exactly() {
        use std::collections::BTreeSet;
        let res = result("homebrew", PublisherGroup::Manager, true);
        let bound: BTreeSet<&str> = failure_var_values(&res, "boom", true, "")
            .iter()
            .map(|(key, _)| *key)
            .collect();
        // Ambient per-crate vars exported on the env channel but bound by
        // the context (per-crate scoping), not by bind_failure_vars.
        let ambient: BTreeSet<&str> = BTreeSet::from(["Tag", "Version"]);
        assert!(
            bound.is_disjoint(&ambient),
            "failure_var_values must not re-bind the ambient per-crate vars"
        );

        let env_side: BTreeSet<&str> = FAILURE_ENV_VARS.iter().map(|(_, var)| *var).collect();
        assert_eq!(
            env_side.len(),
            FAILURE_ENV_VARS.len(),
            "FAILURE_ENV_VARS must not map the same template var twice"
        );
        let expected: BTreeSet<&str> = bound.union(&ambient).copied().collect();
        assert_eq!(
            env_side, expected,
            "FAILURE_ENV_VARS must mirror failure_var_values plus the ambient \
             Tag/Version exactly — adding a var on either side without the \
             matching entry on the other silently drops it from one channel"
        );

        let env_keys: BTreeSet<&str> = FAILURE_ENV_VARS.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            env_keys.len(),
            FAILURE_ENV_VARS.len(),
            "env var names must be unique"
        );
        assert!(
            env_keys.iter().all(|k| k.starts_with("ANODIZER_")),
            "every exported env var must carry the ANODIZER_ prefix"
        );
    }

    #[test]
    fn on_error_exports_failure_context_env_vars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("env.txt");
        let publish = PublishConfig {
            on_error: Some(vec![env_probe_hook(&out, &ALL_ENV_KEYS)]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.2.3")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("homebrew", PublisherGroup::Manager, true);

        fire_on_error(
            &ctx,
            &res,
            "tap push rejected",
            true,
            &run_report_var(&ctx),
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        for expected in [
            "ANODIZER_PUBLISHER=homebrew",
            "ANODIZER_ERROR=tap push rejected",
            "ANODIZER_VERSION=1.2.3",
            "ANODIZER_TAG=v1.2.3",
            "ANODIZER_GROUP=Manager",
            "ANODIZER_REQUIRED=true",
            "ANODIZER_ROLLED_BACK=true",
            // No report was persisted for this synthetic ctx, so the var is
            // exported empty rather than pointing at a stale file.
            "ANODIZER_RUN_REPORT=",
        ] {
            assert!(
                body.lines().any(|l| l == expected),
                "hook env must carry {expected}; got: {body:?}"
            );
        }
    }

    #[test]
    fn error_with_shell_metacharacters_arrives_verbatim_and_does_not_execute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("captured.txt");
        let pwned = dir.path().join("pwned.txt");
        let pwned_sh = pwned.display().to_string().replace('\\', "/");
        // Remote-controlled error text (HTTP body / git stderr shape) carrying
        // every classic shell-injection vector: quote-break, command
        // substitution, backticks.
        let error = format!("'; echo INJECTED > {pwned_sh}; ' `echo BACKTICK` $(echo SUBSHELL)");
        let out_sh = out.display().to_string().replace('\\', "/");
        let publish = PublishConfig {
            on_error: Some(vec![cmd_hook(&format!(
                "printf '%s\\n' \"$ANODIZER_ERROR\" > {out_sh}"
            ))]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        let res = result("mcp-registry", PublisherGroup::Submitter, true);

        fire_on_error(&ctx, &res, &error, false, &run_report_var(&ctx), &log());

        assert!(
            !pwned.exists(),
            "error text must never execute as shell code"
        );
        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim_end_matches('\n'),
            error,
            "the error must arrive in $ANODIZER_ERROR verbatim, unexpanded"
        );
    }

    #[test]
    fn per_crate_mode_env_vars_carry_scoped_version_and_tag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("env.txt");
        let out_sh = out.display().to_string().replace('\\', "/");
        let scoped = PublishConfig {
            on_error: Some(vec![cmd_hook(&format!(
                "printf '%s\\n' \"$ANODIZER_PUBLISHER@$ANODIZER_VERSION/$ANODIZER_TAG\" >> {out_sh}"
            ))]),
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

        fire_on_error(&ctx, &res, "boom", false, &run_report_var(&ctx), &log());

        let body = std::fs::read_to_string(&out).expect("scoped crate hook must run");
        assert_eq!(
            body.trim(),
            "cargo@2.0.0/v2.0.0",
            "env vars must carry the per-crate-scoped Version/Tag"
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

        fire_on_error(&ctx, &res, "boom", false, &run_report_var(&ctx), &log());

        let body = std::fs::read_to_string(&out).expect("scoped crate hook must run");
        assert_eq!(
            body.trim(),
            "cargo@2.0.0",
            "only the in-scope crate's hooks fire; .Version resolves per crate"
        );
    }

    // -----------------------------------------------------------------------
    // on_rollback firing surface
    // -----------------------------------------------------------------------

    /// A clean revert (`rollback_failed = false`) binds the publisher context
    /// on the template surface with `.RollbackFailed = false` and an empty
    /// `.Error`.
    #[test]
    fn on_rollback_fires_with_publisher_group_required_vars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_rollback: Some(vec![probe_hook(
                &out,
                "P={{ .Publisher }} V={{ .Version }} G={{ .Group }} R={{ .Required }} RF={{ .RollbackFailed }} E={{ .Error }} RSN={{ .Reason }}",
            )]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.2.3")
            .crates(vec![crate_with_publish("app", publish)])
            .build();

        fire_on_rollback(
            &ctx,
            "homebrew",
            PublisherGroup::Manager,
            true,
            false,
            "",
            "cargo: publish rejected",
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(
            body.trim(),
            "P=homebrew V=1.2.3 G=Manager R=true RF=false E= RSN=cargo: publish rejected",
            "on_rollback must bind .Publisher/.Version/.Group/.Required/.RollbackFailed/.Error/.Reason"
        );
    }

    /// A failed revert (`rollback_failed = true`) surfaces `.RollbackFailed`
    /// and carries the rollback failure message in `.Error`.
    #[test]
    fn on_rollback_marks_rollback_failed_with_error_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_rollback: Some(vec![probe_hook(
                &out,
                "RF={{ .RollbackFailed }} E={{ .Error }}",
            )]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();

        fire_on_rollback(
            &ctx,
            "homebrew",
            PublisherGroup::Manager,
            true,
            true,
            "tap delete rejected",
            "",
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        assert_eq!(body.trim(), "RF=true E=tap delete rejected");
    }

    const ALL_ROLLBACK_ENV_KEYS: [&str; 8] = [
        "ANODIZER_PUBLISHER",
        "ANODIZER_VERSION",
        "ANODIZER_TAG",
        "ANODIZER_GROUP",
        "ANODIZER_REQUIRED",
        "ANODIZER_ROLLBACK_FAILED",
        "ANODIZER_ERROR",
        "ANODIZER_ROLLBACK_REASON",
    ];

    #[test]
    fn on_rollback_exports_context_env_vars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("env.txt");
        let publish = PublishConfig {
            on_rollback: Some(vec![env_probe_hook(&out, &ALL_ROLLBACK_ENV_KEYS)]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.2.3")
            .crates(vec![crate_with_publish("app", publish)])
            .build();

        fire_on_rollback(
            &ctx,
            "homebrew",
            PublisherGroup::Manager,
            true,
            true,
            "boom",
            "cargo: publish rejected",
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("hook must have written output");
        for expected in [
            "ANODIZER_PUBLISHER=homebrew",
            "ANODIZER_VERSION=1.2.3",
            "ANODIZER_TAG=v1.2.3",
            "ANODIZER_GROUP=Manager",
            "ANODIZER_REQUIRED=true",
            "ANODIZER_ROLLBACK_FAILED=true",
            "ANODIZER_ERROR=boom",
            "ANODIZER_ROLLBACK_REASON=cargo: publish rejected",
        ] {
            assert!(
                body.lines().any(|l| l == expected),
                "hook env must carry {expected}; got: {body:?}"
            );
        }
    }

    /// The env channel must mirror the on_rollback template surface exactly —
    /// the same invariant [`env_channel_mirrors_failure_var_surface_exactly`]
    /// pins for `on_error`. `Version` / `Tag` are ambient (bound by the
    /// context per-crate), so they appear on the env side but not in
    /// `rollback_var_values`.
    #[test]
    fn rollback_env_channel_mirrors_var_surface_exactly() {
        use std::collections::BTreeSet;
        let bound: BTreeSet<&str> = rollback_var_values(
            "homebrew",
            PublisherGroup::Manager,
            true,
            true,
            "boom",
            "cargo: publish rejected",
        )
        .iter()
        .map(|(key, _)| *key)
        .collect();
        let ambient: BTreeSet<&str> = BTreeSet::from(["Tag", "Version"]);
        assert!(
            bound.is_disjoint(&ambient),
            "rollback_var_values must not re-bind the ambient per-crate vars"
        );
        let env_side: BTreeSet<&str> = ROLLBACK_ENV_VARS.iter().map(|(_, var)| *var).collect();
        assert_eq!(
            env_side.len(),
            ROLLBACK_ENV_VARS.len(),
            "ROLLBACK_ENV_VARS must not map the same template var twice"
        );
        let expected: BTreeSet<&str> = bound.union(&ambient).copied().collect();
        assert_eq!(
            env_side, expected,
            "ROLLBACK_ENV_VARS must mirror rollback_var_values plus the ambient \
             Tag/Version exactly — adding a var on either side without the \
             matching entry on the other silently drops it from one channel"
        );
        let env_keys: BTreeSet<&str> = ROLLBACK_ENV_VARS.iter().map(|(key, _)| *key).collect();
        assert_eq!(env_keys.len(), ROLLBACK_ENV_VARS.len(), "env names unique");
        assert!(
            env_keys.iter().all(|k| k.starts_with("ANODIZER_")),
            "every exported env var must carry the ANODIZER_ prefix"
        );
    }

    #[test]
    fn on_rollback_failing_hook_warns_but_does_not_cascade() {
        let publish = PublishConfig {
            on_rollback: Some(vec![cmd_hook("exit 9")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_with_publish("app", publish)])
            .build();
        // Capture the logger so the swallowed failure's observable effect — a
        // logged warning — can be asserted, not just "reached the end".
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        // No cascade: the call has no Result to inspect, so returning normally
        // (rather than panicking/propagating) is itself the no-cascade proof.
        fire_on_rollback(
            &ctx,
            "cargo",
            PublisherGroup::Submitter,
            true,
            false,
            "",
            "",
            &log,
        );
        assert_eq!(
            cap.warn_count(),
            1,
            "a failing on_rollback hook must log exactly one warning"
        );
        assert!(
            cap.warn_messages()
                .iter()
                .any(|m| m.contains("on-rollback") && m.contains("hook failed")),
            "the warning must name the on-rollback hook failure; got: {:?}",
            cap.warn_messages()
        );
    }

    #[test]
    fn on_rollback_dry_run_logs_but_does_not_execute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let publish = PublishConfig {
            on_rollback: Some(vec![probe_hook(&out, "should-not-run")]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .dry_run(true)
            .crates(vec![crate_with_publish("app", publish)])
            .build();

        fire_on_rollback(
            &ctx,
            "homebrew",
            PublisherGroup::Manager,
            true,
            false,
            "",
            "",
            &log(),
        );

        assert!(
            !out.exists(),
            "dry-run must log the hook without executing it"
        );
    }

    /// Per-crate mode: only the in-scope crate's on_rollback hooks fire and
    /// `.Version` resolves from the per-crate template vars — the workspace
    /// per-crate config mode, mirroring the `on_error` per-crate test.
    #[test]
    fn on_rollback_per_crate_mode_resolves_scoped_and_isolated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("fired.txt");
        let scoped = PublishConfig {
            on_rollback: Some(vec![probe_hook(&out, "{{ .Publisher }}@{{ .Version }}")]),
            ..Default::default()
        };
        let other = PublishConfig {
            on_rollback: Some(vec![probe_hook(&out, "WRONG-CRATE")]),
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

        fire_on_rollback(
            &ctx,
            "cargo",
            PublisherGroup::Submitter,
            true,
            false,
            "",
            "",
            &log(),
        );

        let body = std::fs::read_to_string(&out).expect("scoped crate hook must run");
        assert_eq!(
            body.trim(),
            "cargo@2.0.0",
            "only the in-scope crate's on_rollback hooks fire; .Version resolves per crate"
        );
    }
}
