use crate::artifact::Artifact;
use crate::config::{self, BeforePublishArtifactFilter, HookEntry};
use crate::log::StageLogger;
use crate::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use std::process::Command;

/// Redact sensitive environment variable values from output strings.
///
/// Auto-discovers secret-looking env vars using the same heuristics as
/// secret detection: key suffix matching and value prefix matching.
/// This catches both well-known and user-defined secrets.
fn redact_secrets(output: &str) -> String {
    let env: Vec<(String, String)> = std::env::vars().collect();
    crate::redact::string(output, &env)
}

/// Render a hook template string through the full Tera engine.
///
/// Hard-bails on render failure: a typo like `{{ .Teg }}` in a hook command
/// would otherwise execute literal `{{ .Teg }}` and produce a confusing
/// shell error rather than a clear template diagnostic.
fn render_hook_template(template: &str, vars: &TemplateVars, label: &str) -> Result<String> {
    template::render(template, vars)
        .with_context(|| format!("{} hook: render template '{}'", label, template))
}

/// Cross-cutting inputs for [`run_hooks`] that stay constant across every
/// hook in a single invocation. Bundled into one struct so call sites read
/// as named fields instead of a run of positional `bool` / `Option`
/// arguments. Cheap to pass by value — every field is `Copy`.
#[derive(Clone, Copy)]
pub struct HookRunContext<'a> {
    /// Log each hook command instead of executing it.
    pub dry_run: bool,
    /// Sink for status lines and captured command output.
    pub log: &'a StageLogger,
    /// When set, hook `cmd` / `dir` / `env` / `if` are Tera-expanded with
    /// these vars before execution, supporting
    /// conditionals, filters, and `{{ .Env.VAR }}`. `None` runs the literal
    /// command with no rendering.
    pub template_vars: Option<&'a TemplateVars>,
    /// The active build's per-target `builds[].env` map (already
    /// rendered/expanded by the build planner). It layers BETWEEN the
    /// inherited process env (base) and each hook's own `env:` (most
    /// specific) — hook `env:` entries are appended after the build env,
    /// so a key present
    /// in both resolves to the hook value. `None` (the default via
    /// [`HookRunContext::new`]) at non-build hook sites — they have no
    /// build env.
    pub build_env: Option<&'a std::collections::HashMap<String, String>>,
    /// Extra environment variables the hook site injects into the hook
    /// process (e.g. the `ANODIZER_*` failure-context vars set by publish
    /// `on_error` hooks). The env channel exists so untrusted values (error
    /// bodies, remote stderr) reach hooks WITHOUT being interpolated into
    /// the `sh -c` command string, where shell metacharacters would be
    /// command injection. Layered after `build_env` and before each hook's
    /// own `env:` entries, so hook `env:` still wins on key conflicts.
    pub extra_env: Option<&'a [(String, String)]>,
}

impl<'a> HookRunContext<'a> {
    /// Context for a non-build hook run (the common case): no per-target
    /// `build_env`. Build sites construct the struct directly and set
    /// [`HookRunContext::build_env`].
    pub fn new(
        dry_run: bool,
        log: &'a StageLogger,
        template_vars: Option<&'a TemplateVars>,
    ) -> Self {
        Self {
            dry_run,
            log,
            template_vars,
            build_env: None,
            extra_env: None,
        }
    }

    /// Attach site-injected env vars (see [`HookRunContext::extra_env`]).
    pub fn with_extra_env(mut self, extra_env: &'a [(String, String)]) -> Self {
        self.extra_env = Some(extra_env);
        self
    }
}

/// Execute a list of shell hook commands.
///
/// In dry-run mode, log but do not execute.
/// Supports both simple string hooks and structured hooks with cmd/dir/env/output.
///
/// Note: Rust's `Command` inherits the process environment by default.
/// Pipeline env vars (VERSION, TAG, etc.) should be set in the process
/// environment before calling `run_hooks`, which `setup_env()` handles. See
/// [`HookRunContext`] for how `template_vars` and `build_env` are applied.
pub fn run_hooks(hooks: &[HookEntry], label: &str, ctx: HookRunContext<'_>) -> Result<()> {
    run_hooks_inner(hooks, label, ctx, None).map(|_| ())
}

/// First line of a (rendered) hook command, truncated for a single-line status
/// echo. Keeps a multi-line or very long hook from flooding the default log
/// while still naming WHICH hook ran.
fn hook_cmd_summary(cmd: &str) -> String {
    const MAX: usize = 80;
    let first = cmd.lines().next().unwrap_or("").trim();
    if first.chars().count() > MAX {
        let truncated: String = first.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

/// [`run_hooks`] with an optional bound artifact name. When `Some`, this is a
/// per-artifact `before_publish` iteration and the "ran <label> hook" echo is
/// demoted to verbose and names the artifact; `None` keeps the single default
/// status line.
///
/// Returns `true` when at least one entry executed (reached the run past the
/// `if:` gate) and `false` when every entry was `if:`-skipped. Dry-run counts
/// as executed.
fn run_hooks_inner(
    hooks: &[HookEntry],
    label: &str,
    ctx: HookRunContext<'_>,
    artifact_name: Option<&str>,
) -> Result<bool> {
    let HookRunContext {
        dry_run,
        log,
        template_vars,
        build_env,
        extra_env,
    } = ctx;
    let mut executed = false;
    for hook in hooks {
        let (raw_cmd, raw_dir, env, output_flag, if_cond) = match hook {
            HookEntry::Simple(s) => (s.as_str(), None, None, None, None),
            HookEntry::Structured(h) => (
                h.cmd.as_str(),
                h.dir.as_deref(),
                h.env.as_ref(),
                h.output,
                h.if_condition.as_deref(),
            ),
        };

        if let Some(tv) = template_vars {
            let proceed = config::evaluate_if_condition(if_cond, &format!("{label} hook"), |t| {
                render_hook_template(t, tv, label)
            })?;
            if !proceed {
                tracing::debug!(
                    label = label,
                    cmd = raw_cmd,
                    "skipped hook — `if` condition evaluated falsy"
                );
                continue;
            }
        } else if let Some(cond) = if_cond {
            // Without template_vars there's no way to render — treat the gate
            // as proceed unless the literal condition is explicitly falsy.
            // An EMPTY `if:` is the "no gate set" no-op (proceed), matching
            // `evaluate_if_condition`'s `Some("")` → proceed contract used on
            // the template path above; only `false`/`0`/`no` skip here.
            let trimmed = cond.trim();
            let falsy = matches!(trimmed, "false" | "0" | "no");
            if falsy {
                tracing::debug!(
                    label = label,
                    cmd = raw_cmd,
                    "skipped hook — literal `if` condition is falsy"
                );
                continue;
            }
        }

        let cmd_str = if let Some(tv) = template_vars {
            render_hook_template(raw_cmd, tv, label)?
        } else {
            raw_cmd.to_string()
        };

        let dir_str = match raw_dir {
            Some(d) => Some(if let Some(tv) = template_vars {
                render_hook_template(d, tv, label)?
            } else {
                d.to_string()
            }),
            None => None,
        };

        let expanded_env: Option<Vec<(String, String)>> = match env {
            Some(envs) => {
                let pairs = if let Some(tv) = template_vars {
                    config::render_env_entries(envs, |s| render_hook_template(s, tv, label))
                        .with_context(|| format!("{label} hook: render env entries"))?
                } else {
                    config::parse_env_entries(envs)
                        .with_context(|| format!("{label} hook: parse env entries"))?
                };
                Some(pairs)
            }
            None => None,
        };

        executed = true;
        if dry_run {
            log.status(&format!(
                "(dry-run) would run {} hook via `{}`",
                label, cmd_str
            ));
        } else {
            log.verbose(&format!("running {} hook via `{}`", label, cmd_str));
            let mut command = Command::new("sh");
            command.arg("-c").arg(&cmd_str);
            // Hooks inherit the host env so toolchain env vars (PATH, MSVC
            // INCLUDE/LIB, RUSTUP_HOME) flow through. Secret leakage is gated
            // by `redact_secrets` on the output side.
            if let Some(ref d) = dir_str {
                command.current_dir(d);
            }
            // Precedence: process env (inherited, base) < build env
            // < extra env < hook env. Apply the per-target build env first so
            // a same-key hook `env:` entry below overrides it (the
            // `append(buildEnv, hook.Env...)`).
            if let Some(be) = build_env {
                for (k, v) in be {
                    command.env(k, v);
                }
            }
            if let Some(ee) = extra_env {
                for (k, v) in ee {
                    command.env(k, v);
                }
            }
            if let Some(ref envs) = expanded_env {
                for (k, v) in envs {
                    command.env(k, v);
                }
            }
            // Run via the shared helper: captures stdout+stderr, surfaces
            // them on failure, and (at verbose) streams the hook's output
            // live so a long-running before/after hook shows progress. Hook
            // output may contain secrets from the inherited host env, so the
            // helper logger carries the full process env for redaction —
            // matching the prior `redact_secrets` (process-env) coverage,
            // which is broader than the caller logger's attached env.
            let redacting_log = log.clone().with_env(std::env::vars().collect::<Vec<_>>());
            let output = crate::run::run_checked(
                &mut command,
                &redacting_log,
                &format!("{} hook: {}", label, cmd_str),
            )?;

            match artifact_name {
                Some(name) => log.verbose(&format!("ran {label} hook '{cmd_str}' on {name}")),
                None => {
                    // Name WHICH hook ran so several `before:`/`after:` entries
                    // produce distinct default lines instead of N identical
                    // "ran before hook" echoes.
                    let hook_name = hook_cmd_summary(&cmd_str);
                    log.status(&format!("ran {label} hook: {hook_name}"));
                }
            }

            // When output flag is true, echo the hook's (redacted) stdout as a
            // `[hook output]` summary line — but NOT at verbose, where the run
            // helper already teed that stdout live; echoing again would print
            // it twice. The summary exists precisely for the non-verbose case
            // where the live stream is suppressed.
            if output_flag == Some(true) && !log.is_verbose() {
                let redacted_stdout = redact_secrets(&String::from_utf8_lossy(&output.stdout));
                if !redacted_stdout.trim().is_empty() {
                    log.status(&format!("[hook output] {}", redacted_stdout.trim())); // status-ok: opt-in hook stdout summary, only emitted when output: true and non-verbose
                }
            }
        }
    }
    Ok(executed)
}

/// Pipeline stage that runs `config.before_publish.hooks` immediately
/// before any publisher dispatches.
///
/// Sits between the integrity stages (sign / checksum) and the publish
/// phase (release / publish / blob / snapcraft-publish). A non-zero hook
/// exit aborts the release before any publisher writes to a registry,
/// giving operators a last-chance hook for smoke tests, antivirus scans,
/// or external state staging against the staged dist tree.
///
/// Each `hooks[*]` entry runs **once per matching artifact** (the
/// Pro `before_publish:` semantics). For each entry:
///
/// 1. Resolve the entry's `ids:` + `artifacts:` filters and walk
///    `ctx.artifacts.all()` for matches.
/// 2. For each match, bind per-artifact template variables
///    (`ArtifactPath`, `ArtifactName`, `ArtifactExt`, `Os`, `Arch`,
///    plus `ArtifactID` / `ArtifactKind` for parity with the publisher
///    template surface) and render `cmd` / `dir` / `env` / `if`.
/// 3. Execute the rendered hook (or log it under `--dry-run`).
///
/// `HookEntry::Simple` (bare string) implies `artifacts: all` and no
/// `ids:` filter — it runs against every registered artifact. Zero
/// matching artifacts means the hook runs zero times (the lifecycle
/// semantics of `before:` / `after:` do not apply here).
///
/// Honors `--skip=before-publish`: when set, the pipeline's generic
/// skip handling fires before `run` is invoked, so this stage never
/// executes.
pub struct BeforePublishStage;

impl crate::stage::Stage for BeforePublishStage {
    fn name(&self) -> &str {
        "before-publish"
    }

    fn run(&self, ctx: &mut crate::context::Context) -> Result<()> {
        let log = ctx.logger("before-publish");
        let dry_run = ctx.is_dry_run();
        before_publish_stage_inner(ctx, dry_run, &log, &crate::crate_scope::resolve_crate_tag)
    }
}

/// Body of [`BeforePublishStage::run`] with an injectable per-crate tag
/// resolver. Production passes [`crate::crate_scope::resolve_crate_tag`];
/// tests inject a fixed-tag closure so the full stage (global + per-crate
/// passes) can be exercised without a git fixture.
fn before_publish_stage_inner(
    ctx: &mut crate::context::Context,
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&crate::context::Context, &crate::config::CrateConfig) -> Option<String>,
) -> Result<()> {
    // 1. Global `before_publish:` — fires ONCE over the full artifact set
    //    with the run-level template vars (UNCHANGED behavior).
    if let Some(hooks) = ctx
        .config
        .before_publish
        .as_ref()
        .and_then(|c| c.hooks.as_ref())
        .filter(|h| !h.is_empty())
    {
        let base_vars = ctx.template_vars().clone();
        let artifacts: Vec<Artifact> = ctx.artifacts.all().to_vec();
        for entry in hooks {
            run_before_publish_entry(entry, &artifacts, dry_run, log, &base_vars)?;
        }
    }

    // 2. Per-crate `before_publish:` — fires once per selected crate that
    //    declares the block, scoped to that crate's own template vars
    //    (`Version` / `Tag` / `ProjectName`) and restricted to that crate's
    //    artifacts. Works in workspace per-crate mode (each crate's hooks
    //    run before its publishers) and in publish-only per-crate mode
    //    (the stage re-runs per crate, with a single selected crate). A
    //    single-crate / lockstep config with no per-crate block is a no-op.
    run_per_crate_before_publish_with_resolver(ctx, dry_run, log, resolve_tag)
}

/// Which per-crate lifecycle hook block ([`BeforeCrateStage`] /
/// [`AfterCrateStage`]) to run.
#[derive(Clone, Copy)]
enum CrateLifecycleKind {
    /// `crates[].before` — fires at the pipeline HEAD (after version/tag
    /// anchoring, before the build) for each crate.
    Before,
    /// `crates[].after` — fires at the pipeline TAIL (after publish) for
    /// each crate.
    After,
}

impl CrateLifecycleKind {
    /// Stage name + hook label. Matches the top-level `before:` / `after:`
    /// labels so the per-crate and top-level surfaces read identically in
    /// logs and so the pipeline's `--skip=before` / `--skip=after` gate
    /// (keyed on `Stage::name`) suppresses both surfaces with one flag.
    fn label(self) -> &'static str {
        match self {
            CrateLifecycleKind::Before => "before",
            CrateLifecycleKind::After => "after",
        }
    }

    /// The per-crate hook block this kind reads from a [`CrateConfig`].
    fn block(self, c: &crate::config::CrateConfig) -> Option<&crate::config::HooksConfig> {
        match self {
            CrateLifecycleKind::Before => c.before.as_ref(),
            CrateLifecycleKind::After => c.after.as_ref(),
        }
    }
}

/// Pipeline stage that runs each selected crate's `crates[].before` hooks
/// at the HEAD of a full release pipeline (after version/tag anchoring,
/// before the build stage), scoped to that crate's own template vars.
///
/// This is the full-release counterpart of the publish-only per-crate
/// loop's lifecycle hooks (`run_per_crate_lifecycle_hooks`). A plain
/// `anodizer release` on a workspace-per-crate (or lockstep-with-multiple-
/// publisher-crates) config runs as ONE pipeline pass with no Rust-level
/// per-crate loop, so without this stage `crates[].before` would silently
/// no-op there — even though `crates[].before_publish` and the publishers
/// DO iterate per crate internally. This stage closes that gap by
/// iterating the selected crates the SAME way [`BeforePublishStage`] does
/// (`crate::crate_scope::with_crate_scope` + the `selected_crates` filter,
/// empty = all configured crates).
///
/// Single-crate (no `crates:` block) → no-op. Honors `--skip=before` via
/// the pipeline's generic skip gate, keyed on this stage's `name()`.
///
/// Not added to `build_publish_only_pipeline`: publish-only's per-crate
/// loop re-anchors each crate's `Version`/`Tag` from its preserved
/// `context.json` (not a fresh git tag lookup) and fires before/after via
/// `run_per_crate_lifecycle_hooks` against those already-anchored vars.
/// A `with_crate_scope`-based stage would re-resolve the tag from git and
/// could fail or stamp a divergent version, so the two paths are kept
/// distinct by design — see that function's docs and the pipeline builder.
pub struct BeforeCrateStage;

impl crate::stage::Stage for BeforeCrateStage {
    fn name(&self) -> &str {
        "before"
    }

    fn run(&self, ctx: &mut crate::context::Context) -> Result<()> {
        let log = ctx.logger("before");
        let dry_run = ctx.is_dry_run();
        run_per_crate_lifecycle(ctx, CrateLifecycleKind::Before, dry_run, &log)
    }
}

/// Pipeline stage that runs each selected crate's `crates[].after` hooks at
/// the TAIL of a full release pipeline (after publish), scoped to that
/// crate's own template vars. Tail counterpart of [`BeforeCrateStage`];
/// see that stage's docs for the full-release vs publish-only rationale.
///
/// Honors `--skip=after` via the pipeline's generic skip gate.
pub struct AfterCrateStage;

impl crate::stage::Stage for AfterCrateStage {
    fn name(&self) -> &str {
        "after"
    }

    fn run(&self, ctx: &mut crate::context::Context) -> Result<()> {
        let log = ctx.logger("after");
        let dry_run = ctx.is_dry_run();
        run_per_crate_lifecycle(ctx, CrateLifecycleKind::After, dry_run, &log)
    }
}

/// Run the per-crate `before` / `after` lifecycle hooks for every selected
/// crate that declares the block, each rendered against that crate's own
/// scoped template vars (`Version` / `Tag` / `ProjectName`) via
/// [`crate::crate_scope::with_crate_scope`].
///
/// Crate selection mirrors [`run_per_crate_before_publish_with_resolver`]:
/// a non-empty `ctx.options.selected_crates` (explicit `--crate` subset)
/// restricts the set; an empty selection iterates every configured crate. A
/// crate with no matching hook block contributes nothing. Single-crate /
/// lockstep configs with no per-crate block are a no-op.
fn run_per_crate_lifecycle(
    ctx: &mut crate::context::Context,
    kind: CrateLifecycleKind,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    run_per_crate_lifecycle_with_resolver(
        ctx,
        kind,
        dry_run,
        log,
        &crate::crate_scope::resolve_crate_tag,
    )
}

/// [`run_per_crate_lifecycle`] with an injectable tag resolver so tests can
/// exercise the per-crate scoping without a git fixture (production passes
/// [`crate::crate_scope::resolve_crate_tag`]). Mirrors
/// [`run_per_crate_before_publish_with_resolver`].
fn run_per_crate_lifecycle_with_resolver(
    ctx: &mut crate::context::Context,
    kind: CrateLifecycleKind,
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&crate::context::Context, &crate::config::CrateConfig) -> Option<String>,
) -> Result<()> {
    let label = kind.label();
    // Collect the (crate, hooks) pairs up-front so the per-crate scope can
    // take a `&mut Context` without holding a borrow on `ctx.config`.
    let selected = &ctx.options.selected_crates;
    let pending: Vec<(crate::config::CrateConfig, Vec<HookEntry>)> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| {
            kind.block(c)
                .and_then(|b| b.hooks.as_ref())
                .filter(|h| !h.is_empty())
                .map(|h| (c.clone(), h.clone()))
        })
        .collect();

    for (crate_cfg, hooks) in pending {
        crate::crate_scope::with_crate_scope(ctx, &crate_cfg, resolve_tag, |ctx| {
            run_hooks(
                &hooks,
                label,
                HookRunContext::new(dry_run, log, Some(ctx.template_vars())),
            )
        })?;
    }
    Ok(())
}

/// Run the per-crate `before_publish:` hooks for every selected crate that
/// declares the block. Each crate's hooks render against its own scoped
/// template vars (via [`crate::crate_scope::with_crate_scope`]) and iterate
/// only that crate's artifacts.
///
/// Crate selection: when `ctx.options.selected_crates` is non-empty (an
/// explicit `--crate` subset, or the publish-only per-crate loop's single
/// entry) only those crates are considered; otherwise every configured crate
/// is. A crate with no per-crate `before_publish` block contributes nothing.
///
/// Takes an injectable tag resolver so tests can exercise the per-crate
/// scoping without a git fixture (production passes
/// [`crate::crate_scope::resolve_crate_tag`] via
/// [`before_publish_stage_inner`]). Mirrors the `with_published_crate_scope`
/// test seam used by the publisher stages.
fn run_per_crate_before_publish_with_resolver(
    ctx: &mut crate::context::Context,
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&crate::context::Context, &crate::config::CrateConfig) -> Option<String>,
) -> Result<()> {
    // Collect the (crate, hooks) pairs up-front so the per-crate scope can take
    // a `&mut Context` without holding a borrow on `ctx.config`.
    let selected = &ctx.options.selected_crates;
    let pending: Vec<(crate::config::CrateConfig, Vec<HookEntry>)> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| {
            c.before_publish
                .as_ref()
                .and_then(|b| b.hooks.as_ref())
                .filter(|h| !h.is_empty())
                .map(|h| (c.clone(), h.clone()))
        })
        .collect();

    for (crate_cfg, hooks) in pending {
        let crate_name = crate_cfg.name.clone();
        crate::crate_scope::with_crate_scope(ctx, &crate_cfg, resolve_tag, |ctx| {
            let base_vars = ctx.template_vars().clone();
            // Only this crate's artifacts — a per-crate hook must not see a
            // sibling crate's packages (the `ids:` / `artifacts:` filters
            // still apply on top).
            let artifacts: Vec<Artifact> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| a.crate_name == crate_name)
                .cloned()
                .collect();
            for entry in &hooks {
                run_before_publish_entry(entry, &artifacts, dry_run, log, &base_vars)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Iterate `artifacts` for a single `before_publish[*]` entry, executing
/// the hook command once per match with per-artifact template variables
/// bound. Returns `Err` on first hook failure so the pipeline aborts
/// before any publisher dispatches.
fn run_before_publish_entry(
    entry: &HookEntry,
    artifacts: &[Artifact],
    dry_run: bool,
    log: &StageLogger,
    base_vars: &TemplateVars,
) -> Result<()> {
    let (cmd_label, ids_filter, kind_filter, run_once) = match entry {
        HookEntry::Simple(s) => (s.as_str(), None, BeforePublishArtifactFilter::All, false),
        HookEntry::Structured(h) => (
            h.cmd.as_str(),
            h.ids.as_deref(),
            h.artifacts.unwrap_or(BeforePublishArtifactFilter::All),
            h.run_once,
        ),
    };

    // run_once: fire the command a single time with run-level vars, not bound
    // to any artifact. The `ids` / `artifacts` filters and `$ANODIZER_ARTIFACT`
    // are per-artifact concepts that don't apply — the command iterates the
    // dist dir itself. Same single-invocation shape as the run-once `before:`
    // lifecycle hook (one entry, run-level vars, one default status line).
    if run_once {
        let single = std::slice::from_ref(entry);
        run_hooks(
            single,
            "before-publish",
            HookRunContext::new(dry_run, log, Some(base_vars)),
        )?;
        if !dry_run {
            log.status(&format!("ran before-publish hook '{cmd_label}' once")); // status-ok: run-once summary
        }
        return Ok(());
    }

    let mut ran = 0usize;
    for artifact in artifacts {
        if !kind_filter.matches(artifact.kind) {
            continue;
        }
        if let Some(allow_ids) = ids_filter {
            let id = artifact
                .metadata
                .get("id")
                .map(String::as_str)
                .unwrap_or("");
            if !allow_ids.iter().any(|a| a == id) {
                continue;
            }
        }

        let mut vars = base_vars.clone();
        bind_per_artifact_vars(&mut vars, artifact);
        // Reuse the existing single-entry runner so dry-run, output capture,
        // env allow-list, redaction, and `if:` evaluation behave identically
        // to the lifecycle hook sites — only the per-artifact iteration is
        // new here.
        let single = std::slice::from_ref(entry);
        // before_publish hooks are not build hooks — no per-target build env.
        // Naming the artifact demotes the per-iteration "ran" echo to verbose
        // so the default-level result is the one summary line below.
        let executed = run_hooks_inner(
            single,
            "before-publish",
            HookRunContext::new(dry_run, log, Some(&vars)),
            Some(artifact.name()),
        )?;
        // An `if:`-skipped artifact reaches no command, so it must not inflate
        // the summary count (which would contradict the verbose per-artifact
        // lines that only print for executed runs).
        if executed {
            ran += 1;
        }
    }

    if !dry_run && ran > 0 {
        let suffix = if ran == 1 { "" } else { "s" };
        log.status(&format!(
            "ran before-publish hook '{cmd_label}' over {ran} artifact{suffix}"
        )); // status-ok: once-per-hook summary; per-artifact detail is verbose
    }
    Ok(())
}

/// Bind per-artifact template variables onto `vars` for a single
/// `before_publish:` iteration. Mirrors the publisher-side template
/// surface (`crates/cli/src/commands/publisher.rs::build_publisher_command`)
/// so user templates work identically across `publishers:` and
/// `before_publish:`.
fn bind_per_artifact_vars(vars: &mut TemplateVars, artifact: &Artifact) {
    vars.set("ArtifactPath", &artifact.path.to_string_lossy());
    vars.set("ArtifactName", artifact.name());
    vars.set("ArtifactExt", &artifact.ext());
    vars.set("ArtifactKind", artifact.kind.as_str());
    vars.set(
        "ArtifactID",
        artifact
            .metadata
            .get("id")
            .map(String::as_str)
            .unwrap_or(""),
    );
    if let Some(target) = artifact.target.as_deref() {
        let (os, arch) = crate::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    } else {
        vars.set("Os", "");
        vars.set("Arch", "");
        vars.set("Target", "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StructuredHook;
    #[cfg(feature = "test-helpers")]
    use crate::log::LogLevel;
    use crate::log::{StageLogger, Verbosity};
    use std::collections::HashMap;

    fn test_logger() -> StageLogger {
        StageLogger::new("test", Verbosity::Normal)
    }

    fn vars_with_snapshot(is_snapshot: bool) -> TemplateVars {
        let mut v = TemplateVars::new();
        v.set("IsSnapshot", if is_snapshot { "true" } else { "false" });
        v
    }

    fn structured(cmd: &str, if_cond: Option<&str>) -> HookEntry {
        HookEntry::Structured(StructuredHook {
            cmd: cmd.to_string(),
            if_condition: if_cond.map(str::to_string),
            ..Default::default()
        })
    }

    #[test]
    fn hook_if_snapshot_template_runs_on_snapshot() {
        let log = test_logger();
        let vars = vars_with_snapshot(true);
        let hooks = vec![structured("true", Some("{{ IsSnapshot }}"))];
        // dry_run=true → no actual exec; the function still walks the gate.
        run_hooks(&hooks, "test", HookRunContext::new(true, &log, Some(&vars)))
            .expect("snapshot=true must let the hook proceed");
    }

    #[test]
    fn hook_if_snapshot_template_skips_when_not_snapshot() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured(
            "false-cmd-must-be-skipped",
            Some("{{ IsSnapshot }}"),
        )];
        run_hooks(
            &hooks,
            "test",
            HookRunContext::new(false, &log, Some(&vars)),
        )
        .expect("falsy `if:` must skip without spawning the cmd");
    }

    #[test]
    fn hook_if_literal_true_always_runs() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some("true"))];
        run_hooks(&hooks, "test", HookRunContext::new(true, &log, Some(&vars)))
            .expect("`if: true` must proceed");
    }

    #[test]
    fn hook_if_empty_literal_is_noop_gate() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some(""))];
        run_hooks(&hooks, "test", HookRunContext::new(true, &log, Some(&vars)))
            .expect("empty `if:` literal must be a no-op (always proceed)");
    }

    #[test]
    fn hook_if_empty_literal_no_vars_proceeds() {
        // The no-`template_vars` branch must treat an empty `if:` as
        // "no gate set" → proceed, identical to the template path and to
        // `evaluate_if_condition`'s `Some("")` contract. A dry-run hook that
        // would loudly fail if executed proves the gate let it through (the
        // command is logged, never spawned, under dry-run).
        let log = test_logger();
        let hooks = vec![structured("true", Some(""))];
        run_hooks(&hooks, "test", HookRunContext::new(true, &log, None))
            .expect("empty `if:` with no vars must proceed (no-op gate)");
    }

    #[test]
    fn hook_if_falsy_literal_no_vars_skips() {
        // The complementary pin: an explicitly-falsy literal still skips on
        // the no-vars path. `false-cmd` would error if spawned; a non-dry-run
        // call succeeding proves it was skipped, not executed.
        let log = test_logger();
        let hooks = vec![structured("false-cmd-must-be-skipped", Some("false"))];
        run_hooks(&hooks, "test", HookRunContext::new(false, &log, None))
            .expect("falsy literal `if:` with no vars must skip without spawning");
    }

    /// Run a single real (non-dry-run) hook that appends `KEY=$KEY` lines to
    /// `out_file` for each requested key, so the caller can assert what the
    /// hook actually saw in its process environment.
    fn run_env_probe_hook(
        out_file: &std::path::Path,
        keys: &[&str],
        hook_env: Option<Vec<String>>,
        build_env: Option<&HashMap<String, String>>,
    ) -> Result<()> {
        let log = test_logger();
        // sh -c mangles backslashes; feed it a forward-slash path so the redirect target resolves on Windows
        let out = out_file.display().to_string().replace('\\', "/");
        let probe = keys
            .iter()
            .map(|k| format!("echo {k}=${k} >> {out}"))
            .collect::<Vec<_>>()
            .join("; ");
        let hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: probe,
            env: hook_env,
            ..Default::default()
        })];
        let vars = TemplateVars::new();
        run_hooks(
            &hooks,
            "build",
            HookRunContext {
                dry_run: false,
                log: &log,
                template_vars: Some(&vars),
                build_env,
                extra_env: None,
            },
        )
    }

    #[test]
    fn build_env_reaches_build_hook() {
        let dir = std::env::temp_dir().join(format!("anodizer-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("reaches.txt");
        let _ = std::fs::remove_file(&out);

        let mut build_env = HashMap::new();
        build_env.insert("MY_BUILD_VAR".to_string(), "from-build-env".to_string());

        run_env_probe_hook(&out, &["MY_BUILD_VAR"], None, Some(&build_env)).expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("MY_BUILD_VAR=from-build-env"),
            "build env var must reach the hook; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn hook_env_overrides_build_env_on_key_conflict() {
        let dir = std::env::temp_dir().join(format!("anodizer-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("precedence.txt");
        let _ = std::fs::remove_file(&out);

        let mut build_env = HashMap::new();
        build_env.insert("SHARED".to_string(), "build-loses".to_string());

        run_env_probe_hook(
            &out,
            &["SHARED"],
            Some(vec!["SHARED=hook-wins".to_string()]),
            Some(&build_env),
        )
        .expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("SHARED=hook-wins"),
            "hook env must override build env on key conflict (GR append order); got: {contents:?}"
        );
        assert!(
            !contents.contains("SHARED=build-loses"),
            "build env value must not survive a hook-env override; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// Like [`run_env_probe_hook`] but exercises the `extra_env` channel.
    fn run_extra_env_probe_hook(
        out_file: &std::path::Path,
        keys: &[&str],
        hook_env: Option<Vec<String>>,
        extra_env: &[(String, String)],
    ) -> Result<()> {
        let log = test_logger();
        let out = out_file.display().to_string().replace('\\', "/");
        let probe = keys
            .iter()
            .map(|k| format!("echo {k}=${k} >> {out}"))
            .collect::<Vec<_>>()
            .join("; ");
        let hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: probe,
            env: hook_env,
            ..Default::default()
        })];
        let vars = TemplateVars::new();
        run_hooks(
            &hooks,
            "test",
            HookRunContext::new(false, &log, Some(&vars)).with_extra_env(extra_env),
        )
    }

    #[test]
    fn extra_env_reaches_hook() {
        let dir = std::env::temp_dir().join(format!("anodizer-ee-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("reaches.txt");
        let _ = std::fs::remove_file(&out);

        let extra = vec![("MY_EXTRA_VAR".to_string(), "from-extra-env".to_string())];
        run_extra_env_probe_hook(&out, &["MY_EXTRA_VAR"], None, &extra).expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("MY_EXTRA_VAR=from-extra-env"),
            "extra env var must reach the hook; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn hook_env_overrides_extra_env_on_key_conflict() {
        let dir = std::env::temp_dir().join(format!("anodizer-ee-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("precedence.txt");
        let _ = std::fs::remove_file(&out);

        let extra = vec![("SHARED".to_string(), "extra-loses".to_string())];
        run_extra_env_probe_hook(
            &out,
            &["SHARED"],
            Some(vec!["SHARED=hook-wins".to_string()]),
            &extra,
        )
        .expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("SHARED=hook-wins"),
            "hook env must override extra env on key conflict; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn absent_build_env_is_unchanged_behavior() {
        let dir = std::env::temp_dir().join(format!("anodizer-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("absent.txt");
        let _ = std::fs::remove_file(&out);

        // No build env at all, and the probed key is unset → empty value, no panic.
        run_env_probe_hook(&out, &["NOT_SET_ANYWHERE"], None, None).expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("NOT_SET_ANYWHERE="),
            "absent build env must leave behavior unchanged; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn empty_build_env_map_adds_nothing() {
        let dir = std::env::temp_dir().join(format!("anodizer-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("empty.txt");
        let _ = std::fs::remove_file(&out);

        let build_env: HashMap<String, String> = HashMap::new();
        run_env_probe_hook(&out, &["NOT_SET_ANYWHERE"], None, Some(&build_env))
            .expect("hook must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("NOT_SET_ANYWHERE="),
            "empty build env map must be a no-op; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn hook_if_render_error_propagates() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some("{{ UndefinedSymbol }}"))];
        let err = run_hooks(&hooks, "test", HookRunContext::new(true, &log, Some(&vars)))
            .expect_err("unrenderable template must surface as Err");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("template render failed") || chain.contains("UndefinedSymbol"),
            "expected render-error diagnostic, got: {chain}",
        );
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn hook_output_summary_suppressed_at_verbose() {
        // With `output: true` the run helper tees the hook's stdout live at
        // verbose, so the `[hook output]` status summary must NOT also fire —
        // otherwise the same line prints twice. The capture records the status
        // lines; none may carry the `[hook output]` prefix at verbose.
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
        let hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: "echo HOOKSTDOUTLINE".to_string(),
            output: Some(true),
            ..Default::default()
        })];
        let vars = TemplateVars::new();
        run_hooks(
            &hooks,
            "test",
            HookRunContext::new(false, &log, Some(&vars)),
        )
        .expect("hook must run");
        let summarized = cap
            .all_messages()
            .into_iter()
            .any(|(_, m)| m.contains("[hook output]"));
        assert!(
            !summarized,
            "at verbose the live tee owns the hook stdout; the [hook output] \
             summary must be suppressed to avoid a double print"
        );
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn hook_output_summary_present_at_normal() {
        // The complementary pin: at non-verbose verbosity the live stream is
        // suppressed, so the `[hook output]` summary IS the only surface for
        // the hook's stdout and must still fire.
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: "echo HOOKSTDOUTLINE".to_string(),
            output: Some(true),
            ..Default::default()
        })];
        let vars = TemplateVars::new();
        run_hooks(
            &hooks,
            "test",
            HookRunContext::new(false, &log, Some(&vars)),
        )
        .expect("hook must run");
        let summarized = cap
            .all_messages()
            .into_iter()
            .any(|(_, m)| m.contains("[hook output]") && m.contains("HOOKSTDOUTLINE"));
        assert!(
            summarized,
            "at normal verbosity the [hook output] summary must carry the hook's stdout"
        );
    }

    // ── per-crate before_publish ────────────────────────────────────────

    use crate::artifact::{Artifact, ArtifactKind};
    use crate::config::{Config, CrateConfig, HooksConfig};
    use crate::context::{Context, ContextOptions};

    /// A crate config carrying a per-crate `before_publish` hook that writes
    /// the rendered `{{ Version }}` and each `{{ ArtifactName }}` it sees to
    /// `out_file` — so a test can assert which version-scope and which
    /// artifacts the hook observed.
    fn crate_with_before_publish(name: &str, out_file: &str) -> CrateConfig {
        let probe = format!("echo {name}:{{{{ Version }}}}:{{{{ ArtifactName }}}} >> {out_file}");
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            before_publish: Some(HooksConfig {
                hooks: Some(vec![HookEntry::Simple(probe)]),
                post: None,
            }),
            ..Default::default()
        }
    }

    fn pkg_artifact(crate_name: &str, file_name: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::LinuxPackage,
            path: std::path::PathBuf::from(file_name),
            name: file_name.to_string(),
            target: None,
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    /// Fixed-tag resolver: each crate gets a distinct version so the test can
    /// prove the per-crate scope re-anchors `Version` per crate.
    fn fixed_tag(_ctx: &Context, c: &CrateConfig) -> Option<String> {
        match c.name.as_str() {
            "foo" => Some("v1.2.3".to_string()),
            "bar" => Some("v9.9.9".to_string()),
            _ => Some("v0.0.0".to_string()),
        }
    }

    /// Per-crate `before_publish` fires once per crate, scoped to that crate's
    /// own `Version` and restricted to that crate's artifacts. The global
    /// `before_publish` (absent here) does not fire.
    #[test]
    fn per_crate_before_publish_scopes_version_and_artifacts() {
        let dir = std::env::temp_dir().join(format!("anodizer-pcbp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("scoped.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_before_publish("foo", &out_s),
                crate_with_before_publish("bar", &out_s),
            ],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.artifacts.add(pkg_artifact("foo", "foo_1.2.3.deb"));
        ctx.artifacts.add(pkg_artifact("bar", "bar_9.9.9.deb"));

        let log = ctx.logger("before-publish");
        run_per_crate_before_publish_with_resolver(&mut ctx, false, &log, &fixed_tag)
            .expect("per-crate before_publish must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        // foo's hook ran with foo's version, seeing only foo's artifact.
        assert!(
            contents.contains("foo:1.2.3:foo_1.2.3.deb"),
            "foo hook must be scoped to foo's version + artifact; got: {contents:?}"
        );
        // bar's hook ran with bar's version, seeing only bar's artifact.
        assert!(
            contents.contains("bar:9.9.9:bar_9.9.9.deb"),
            "bar hook must be scoped to bar's version + artifact; got: {contents:?}"
        );
        // No cross-contamination: foo's hook never saw bar's artifact.
        assert!(
            !contents.contains("foo:1.2.3:bar_9.9.9.deb"),
            "foo hook must NOT see bar's artifact; got: {contents:?}"
        );
        assert!(
            !contents.contains("bar:9.9.9:foo_1.2.3.deb"),
            "bar hook must NOT see foo's artifact; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// With a non-empty `selected_crates`, only the selected crate's per-crate
    /// `before_publish` fires — the publish-only per-crate loop sets a single
    /// selected crate, so a sibling's hooks must not leak into that iteration.
    #[test]
    fn per_crate_before_publish_honors_selected_crates() {
        let dir = std::env::temp_dir().join(format!("anodizer-pcbp-sel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("selected.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_before_publish("foo", &out_s),
                crate_with_before_publish("bar", &out_s),
            ],
            ..Default::default()
        };
        let opts = ContextOptions {
            selected_crates: vec!["foo".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.artifacts.add(pkg_artifact("foo", "foo_1.2.3.deb"));
        ctx.artifacts.add(pkg_artifact("bar", "bar_9.9.9.deb"));

        let log = ctx.logger("before-publish");
        run_per_crate_before_publish_with_resolver(&mut ctx, false, &log, &fixed_tag)
            .expect("selected per-crate before_publish must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("foo:1.2.3:foo_1.2.3.deb"),
            "selected crate foo's hook must fire; got: {contents:?}"
        );
        assert!(
            !contents.contains("bar:"),
            "unselected crate bar's hook must NOT fire; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// A config with NO per-crate `before_publish` block is a no-op (single
    /// crate / lockstep parity — the global block, if any, still fires
    /// separately in `run`).
    #[test]
    fn per_crate_before_publish_noop_without_block() {
        let config = Config {
            crates: vec![CrateConfig {
                name: "foo".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.artifacts.add(pkg_artifact("foo", "foo.deb"));
        let log = ctx.logger("before-publish");
        // No hooks → must succeed without spawning anything.
        run_per_crate_before_publish_with_resolver(&mut ctx, false, &log, &fixed_tag)
            .expect("absent per-crate before_publish must be a no-op");
    }

    /// Full `BeforePublishStage::run` path (via the resolver-injectable inner)
    /// with an EMPTY `selected_crates`: every configured crate's per-crate
    /// `before_publish` must fire, each scoped to its own version and
    /// artifacts. This is the full-release path — no `--crate` subset, so the
    /// implicit-all selection iterates every crate.
    #[test]
    fn before_publish_stage_empty_selection_fires_every_crate() {
        let dir = std::env::temp_dir().join(format!("anodizer-bps-all-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("all.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_before_publish("foo", &out_s),
                crate_with_before_publish("bar", &out_s),
            ],
            ..Default::default()
        };
        // ContextOptions::default() leaves selected_crates empty → implicit-all.
        let mut ctx = Context::new(config, ContextOptions::default());
        assert!(
            ctx.options.selected_crates.is_empty(),
            "this test exercises the empty-selection (full-release) path"
        );
        ctx.artifacts.add(pkg_artifact("foo", "foo_1.2.3.deb"));
        ctx.artifacts.add(pkg_artifact("bar", "bar_9.9.9.deb"));

        let log = ctx.logger("before-publish");
        before_publish_stage_inner(&mut ctx, false, &log, &fixed_tag)
            .expect("full before-publish stage must run with empty selection");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(
            contents.contains("foo:1.2.3:foo_1.2.3.deb"),
            "every crate's before_publish must fire on the full-release path; \
             foo missing in: {contents:?}"
        );
        assert!(
            contents.contains("bar:9.9.9:bar_9.9.9.deb"),
            "every crate's before_publish must fire on the full-release path; \
             bar missing in: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn before_publish_entry_emits_one_summary_not_per_artifact() {
        let (log, cap) = StageLogger::with_capture("before-publish", Verbosity::Verbose);
        let artifacts = vec![
            pkg_artifact("foo", "a.deb"),
            pkg_artifact("foo", "b.deb"),
            pkg_artifact("foo", "c.deb"),
        ];
        let base = TemplateVars::new();
        run_before_publish_entry(
            &HookEntry::Simple("true".to_string()),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect("entry must run over every artifact");

        let msgs = cap.all_messages();
        let summary_status: Vec<&String> = msgs
            .iter()
            .filter(|(lvl, m)| {
                *lvl == LogLevel::Status && m.contains("before-publish hook") && m.contains('3')
            })
            .map(|(_, m)| m)
            .collect();
        assert_eq!(
            summary_status.len(),
            1,
            "exactly one default-level summary line for 3 artifacts; got: {:?}",
            msgs
        );
        let ran_status = msgs
            .iter()
            .filter(|(lvl, m)| *lvl == LogLevel::Status && m.starts_with("ran "))
            .count();
        assert_eq!(
            ran_status, 1,
            "no per-artifact 'ran' line may reach default level; got: {msgs:?}"
        );
        let per_artifact_verbose = msgs
            .iter()
            .filter(|(lvl, m)| {
                *lvl == LogLevel::Verbose && m.starts_with("ran ") && m.contains(".deb")
            })
            .count();
        assert_eq!(
            per_artifact_verbose, 3,
            "each artifact's 'ran' detail must be verbose-only and name the artifact; got: {msgs:?}"
        );
    }

    /// The summary counts only artifacts the hook actually ran on, not `if:`-skipped ones.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn before_publish_summary_excludes_if_skipped_artifacts() {
        let (log, cap) = StageLogger::with_capture("before-publish", Verbosity::Verbose);
        let artifacts = vec![
            pkg_artifact("foo", "a.deb"),
            pkg_artifact("foo", "b.deb"),
            pkg_artifact("foo", "c.deb"),
        ];
        let base = TemplateVars::new();
        // Truthy only for `b.deb`; the other two are `if:`-skipped.
        run_before_publish_entry(
            &HookEntry::Structured(StructuredHook {
                cmd: "true".to_string(),
                if_condition: Some("{{ ArtifactName == \"b.deb\" }}".to_string()),
                ..Default::default()
            }),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect("entry must run");

        let msgs = cap.all_messages();
        let summary: Vec<&String> = msgs
            .iter()
            .filter(|(lvl, m)| *lvl == LogLevel::Status && m.contains("before-publish hook"))
            .map(|(_, m)| m)
            .collect();
        assert_eq!(
            summary.len(),
            1,
            "exactly one default-level summary; got: {msgs:?}"
        );
        assert!(
            summary[0].contains("over 1 artifact") && !summary[0].contains("over 1 artifacts"),
            "summary must count only the one executed artifact (singular), not the 3 filtered-in; \
             got: {:?}",
            summary[0]
        );
        let per_artifact_verbose: Vec<&String> = msgs
            .iter()
            .filter(|(lvl, m)| {
                *lvl == LogLevel::Verbose && m.starts_with("ran ") && m.contains(".deb")
            })
            .map(|(_, m)| m)
            .collect();
        assert_eq!(
            per_artifact_verbose.len(),
            1,
            "only the executed artifact emits a verbose 'ran ... on <name>' line; got: {msgs:?}"
        );
        assert!(
            per_artifact_verbose[0].contains("b.deb"),
            "the one verbose line must name the artifact that actually ran; got: {:?}",
            per_artifact_verbose[0]
        );
    }

    /// `run_once: true` executes the command exactly once regardless of how many artifacts match.
    #[test]
    fn before_publish_run_once_executes_a_single_time_for_many_artifacts() {
        let dir = std::env::temp_dir().join(format!("anodizer-bp-runonce-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("runs.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let log = test_logger();
        let artifacts = vec![
            pkg_artifact("foo", "a.deb"),
            pkg_artifact("foo", "b.deb"),
            pkg_artifact("foo", "c.deb"),
        ];
        let mut base = TemplateVars::new();
        base.set("Version", "1.2.3");
        run_before_publish_entry(
            &HookEntry::Structured(StructuredHook {
                cmd: format!("echo once:{{{{ Version }}}} >> {out_s}"),
                run_once: true,
                ..Default::default()
            }),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect("run_once entry must execute");

        let contents = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "run_once hook must fire exactly once for 3 artifacts; got: {contents:?}"
        );
        assert_eq!(
            lines[0], "once:1.2.3",
            "run_once hook must render run-level vars (Version), not per-artifact vars; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// A `run_once` hook whose command exits non-zero must abort the stage
    /// (return `Err`), exactly like a per-artifact hook failure — so a failing
    /// integrity gate still blocks the publish.
    #[test]
    fn before_publish_run_once_nonzero_exit_fails_stage() {
        let log = test_logger();
        let artifacts = vec![pkg_artifact("foo", "a.deb"), pkg_artifact("foo", "b.deb")];
        let base = TemplateVars::new();
        let err = run_before_publish_entry(
            &HookEntry::Structured(StructuredHook {
                cmd: "exit 1".to_string(),
                run_once: true,
                ..Default::default()
            }),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect_err("a non-zero run_once command must fail the stage");
        let _ = err;
    }

    /// `run_once: true` ignores the per-artifact `ids` / `artifacts` filters and still runs once.
    #[test]
    fn before_publish_run_once_ignores_artifact_filters() {
        let dir =
            std::env::temp_dir().join(format!("anodizer-bp-runonce-flt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("flt.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let log = test_logger();
        // Only LinuxPackage artifacts exist; filter asks for Checksum (no match).
        let artifacts = vec![pkg_artifact("foo", "a.deb")];
        let base = TemplateVars::new();
        run_before_publish_entry(
            &HookEntry::Structured(StructuredHook {
                cmd: format!("echo ran >> {out_s}"),
                run_once: true,
                artifacts: Some(BeforePublishArtifactFilter::Checksum),
                ids: Some(vec!["nonexistent".to_string()]),
                ..Default::default()
            }),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect("run_once entry must run despite non-matching filters");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            contents.lines().filter(|l| !l.is_empty()).count(),
            1,
            "run_once must fire exactly once regardless of artifact filters; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// `run_once: false` (and absent) runs the command once per matching artifact.
    #[test]
    fn before_publish_run_once_false_stays_per_artifact() {
        let dir = std::env::temp_dir().join(format!("anodizer-bp-perart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("perart.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let log = test_logger();
        let artifacts = vec![
            pkg_artifact("foo", "a.deb"),
            pkg_artifact("foo", "b.deb"),
            pkg_artifact("foo", "c.deb"),
        ];
        let base = TemplateVars::new();
        run_before_publish_entry(
            &HookEntry::Structured(StructuredHook {
                cmd: format!("echo {{{{ ArtifactName }}}} >> {out_s}"),
                run_once: false,
                ..Default::default()
            }),
            &artifacts,
            false,
            &log,
            &base,
        )
        .expect("per-artifact entry must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        let mut lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec!["a.deb", "b.deb", "c.deb"],
            "run_once:false must fire once per artifact with per-artifact vars; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    // ── per-crate before / after lifecycle stages ───────────────────────────

    /// A crate config carrying a per-crate `before` (or `after`) lifecycle
    /// hook that appends `<phase>:<name>:{{ Version }}` to `out_file`, so a
    /// test can assert which phase fired, for which crate, under which
    /// version scope. `phase` is the label written by the hook ("before" /
    /// "after"); whether it lands in `.before` or `.after` is the caller's
    /// choice via `set_before`.
    fn crate_with_lifecycle(name: &str, out_file: &str, set_before: bool) -> CrateConfig {
        let phase = if set_before { "before" } else { "after" };
        let probe = format!("echo {phase}:{name}:{{{{ Version }}}} >> {out_file}");
        let hooks = Some(HooksConfig {
            hooks: Some(vec![HookEntry::Simple(probe)]),
            post: None,
        });
        let mut c = CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        };
        if set_before {
            c.before = hooks;
        } else {
            c.after = hooks;
        }
        c
    }

    /// `crates[].before` fires once per crate on the full-release path (empty
    /// selection), each scoped to that crate's own `Version`. This is the gap
    /// the new stage closes: a workspace-per-crate / lockstep-multi-crate full
    /// release previously no-op'd these hooks.
    #[test]
    fn per_crate_before_fires_once_per_crate_full_release() {
        let dir = std::env::temp_dir().join(format!("anodizer-pcb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("before.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_lifecycle("foo", &out_s, true),
                crate_with_lifecycle("bar", &out_s, true),
            ],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("before");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::Before,
            false,
            &log,
            &fixed_tag,
        )
        .expect("per-crate before must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        // Exactly two firings — one per crate, no double-fire.
        assert_eq!(
            contents.matches("before:").count(),
            2,
            "before must fire exactly once per crate (no double-fire); got: {contents:?}"
        );
        assert!(
            contents.contains("before:foo:1.2.3"),
            "foo's before must render under foo's version; got: {contents:?}"
        );
        assert!(
            contents.contains("before:bar:9.9.9"),
            "bar's before must render under bar's version; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// `crates[].after` fires once per crate on the full-release path, scoped
    /// per crate — the tail counterpart of the before test.
    #[test]
    fn per_crate_after_fires_once_per_crate_full_release() {
        let dir = std::env::temp_dir().join(format!("anodizer-pca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("after.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_lifecycle("foo", &out_s, false),
                crate_with_lifecycle("bar", &out_s, false),
            ],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("after");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::After,
            false,
            &log,
            &fixed_tag,
        )
        .expect("per-crate after must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            contents.matches("after:").count(),
            2,
            "after must fire exactly once per crate; got: {contents:?}"
        );
        assert!(
            contents.contains("after:foo:1.2.3") && contents.contains("after:bar:9.9.9"),
            "each crate's after must render under its own version; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// Lockstep-with-multiple-publisher-crates: every crate shares one version
    /// (the `fixed_lockstep` resolver hands all crates the same tag), and
    /// `before` still fires once per crate — parity with `before_publish` /
    /// the publishers, which iterate per crate in lockstep too.
    #[test]
    fn per_crate_before_fires_per_crate_in_lockstep() {
        fn fixed_lockstep(_ctx: &Context, _c: &CrateConfig) -> Option<String> {
            Some("v2.0.0".to_string())
        }
        let dir = std::env::temp_dir().join(format!("anodizer-pcl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("lockstep.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_lifecycle("foo", &out_s, true),
                crate_with_lifecycle("bar", &out_s, true),
            ],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("before");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::Before,
            false,
            &log,
            &fixed_lockstep,
        )
        .expect("per-crate before must run in lockstep");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            contents.matches("before:").count(),
            2,
            "lockstep multi-crate must still fire before once per crate; got: {contents:?}"
        );
        assert!(
            contents.contains("before:foo:2.0.0") && contents.contains("before:bar:2.0.0"),
            "both crates share the lockstep version; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// With a non-empty `selected_crates` (the publish-only per-crate loop's
    /// single entry, or an explicit `--crate` subset), only the selected
    /// crate's per-crate `before` fires — proving no sibling leak AND that
    /// firing exactly the single selected crate's hook once is what
    /// publish-only's per-crate loop would produce (one stage invocation per
    /// crate, each with `selected_crates=[one]`).
    #[test]
    fn per_crate_before_honors_selected_crates_single_fire() {
        let dir = std::env::temp_dir().join(format!("anodizer-pcb-sel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("selected.txt");
        let _ = std::fs::remove_file(&out);
        let out_s = out.display().to_string().replace('\\', "/");

        let config = Config {
            crates: vec![
                crate_with_lifecycle("foo", &out_s, true),
                crate_with_lifecycle("bar", &out_s, true),
            ],
            ..Default::default()
        };
        let opts = ContextOptions {
            selected_crates: vec!["foo".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let log = ctx.logger("before");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::Before,
            false,
            &log,
            &fixed_tag,
        )
        .expect("selected per-crate before must run");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            contents.matches("before:").count(),
            1,
            "exactly the single selected crate's hook fires once; got: {contents:?}"
        );
        assert!(
            contents.contains("before:foo:1.2.3"),
            "selected crate foo's before must fire; got: {contents:?}"
        );
        assert!(
            !contents.contains("bar"),
            "unselected crate bar's before must NOT fire; got: {contents:?}"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// A config with NO per-crate `before` / `after` block is a no-op
    /// (single-crate / lockstep parity — the top-level `before:` / `after:`
    /// fire separately in the dispatcher).
    #[test]
    fn per_crate_lifecycle_noop_without_block() {
        let config = Config {
            crates: vec![CrateConfig {
                name: "foo".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("before");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::Before,
            false,
            &log,
            &fixed_tag,
        )
        .expect("absent per-crate before must be a no-op");
        run_per_crate_lifecycle_with_resolver(
            &mut ctx,
            CrateLifecycleKind::After,
            false,
            &log,
            &fixed_tag,
        )
        .expect("absent per-crate after must be a no-op");
    }

    #[test]
    fn hook_cmd_summary_takes_first_line_and_truncates() {
        // Short single-line: returned verbatim (trimmed).
        assert_eq!(
            hook_cmd_summary("  cargo fmt --check  "),
            "cargo fmt --check"
        );
        // Multi-line: only the first line names the hook.
        assert_eq!(
            hook_cmd_summary("cargo build\ncargo test\ncargo clippy"),
            "cargo build"
        );
        // Over the cap: truncated with an ellipsis, never flooding the log.
        let long = "x".repeat(200);
        let summary = hook_cmd_summary(&long);
        assert!(
            summary.ends_with('…'),
            "long command must be elided: {summary}"
        );
        assert_eq!(summary.chars().count(), 81, "80 chars + the ellipsis");
    }
}
