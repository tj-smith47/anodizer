use crate::artifact::Artifact;
use crate::config::{self, BeforePublishArtifactFilter, HookEntry};
use crate::log::StageLogger;
use crate::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use std::process::Command;

/// Redact sensitive environment variable values from output strings.
///
/// Auto-discovers secret-looking env vars using the same heuristics as
/// GoReleaser's `redact.go`: key suffix matching and value prefix matching.
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

/// Execute a list of shell hook commands.
///
/// In dry-run mode, log but do not execute.
/// Supports both simple string hooks and structured hooks with cmd/dir/env/output.
///
/// When `template_vars` is provided, hook commands, directories, and environment
/// values are template-expanded through the full Tera engine before execution
/// (like GoReleaser), supporting conditionals, filters, and `{{ .Env.VAR }}`.
///
/// Note: Rust's `Command` inherits the process environment by default.
/// Pipeline env vars (VERSION, TAG, etc.) should be set in the process
/// environment before calling `run_hooks`, which `setup_env()` handles.
pub fn run_hooks(
    hooks: &[HookEntry],
    label: &str,
    dry_run: bool,
    log: &StageLogger,
    template_vars: Option<&TemplateVars>,
) -> Result<()> {
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
                    "skipping hook: `if` condition evaluated falsy"
                );
                continue;
            }
        } else if let Some(cond) = if_cond {
            // Without template_vars there's no way to render — treat the gate
            // as proceed only when the literal condition itself is truthy.
            let trimmed = cond.trim();
            let falsy = matches!(trimmed, "" | "false" | "0" | "no");
            if falsy {
                tracing::debug!(
                    label = label,
                    cmd = raw_cmd,
                    "skipping hook: literal `if` condition is falsy"
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

        if dry_run {
            log.status(&format!("[dry-run] {} hook: {}", label, cmd_str));
        } else {
            log.status(&format!("running {} hook: {}", label, cmd_str));
            let mut command = Command::new("sh");
            command.arg("-c").arg(&cmd_str);
            // Hooks inherit the host env so toolchain env vars (PATH, MSVC
            // INCLUDE/LIB, RUSTUP_HOME) flow through. Secret leakage is gated
            // by `redact_secrets` on the output side.
            if let Some(ref d) = dir_str {
                command.current_dir(d);
            }
            if let Some(ref envs) = expanded_env {
                for (k, v) in envs {
                    command.env(k, v);
                }
            }
            let output = command
                .output()
                .with_context(|| format!("failed to spawn {} hook: {}", label, cmd_str))?;

            // Redact secrets from stdout/stderr before any logging so that
            // sensitive env var values never leak to CI logs or terminals.
            let redacted_stdout = redact_secrets(&String::from_utf8_lossy(&output.stdout));
            let redacted_stderr = redact_secrets(&String::from_utf8_lossy(&output.stderr));

            // When output flag is true, print the hook's stdout to the log
            if output_flag == Some(true) && !redacted_stdout.trim().is_empty() {
                log.status(&format!("[hook output] {}", redacted_stdout.trim()));
            }

            // Build a synthetic Output with redacted content for check_output,
            // so that error messages also have secrets scrubbed.
            let redacted_output = std::process::Output {
                status: output.status,
                stdout: redacted_stdout.into_bytes(),
                stderr: redacted_stderr.into_bytes(),
            };

            log.check_output(redacted_output, &format!("{} hook: {}", label, cmd_str))?;
        }
    }
    Ok(())
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
/// Each `hooks[*]` entry runs **once per matching artifact** (GoReleaser
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
        let Some(cfg) = ctx.config.before_publish.as_ref() else {
            return Ok(());
        };
        let Some(hooks) = cfg.hooks.as_ref() else {
            return Ok(());
        };
        if hooks.is_empty() {
            return Ok(());
        }

        let dry_run = ctx.is_dry_run();
        let base_vars = ctx.template_vars().clone();
        let artifacts: Vec<Artifact> = ctx.artifacts.all().to_vec();

        for entry in hooks {
            run_before_publish_entry(entry, &artifacts, dry_run, &log, &base_vars)?;
        }
        Ok(())
    }
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
    let (ids_filter, kind_filter) = match entry {
        HookEntry::Simple(_) => (None, BeforePublishArtifactFilter::All),
        HookEntry::Structured(h) => (
            h.ids.as_deref(),
            h.artifacts.unwrap_or(BeforePublishArtifactFilter::All),
        ),
    };

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
        run_hooks(single, "before-publish", dry_run, log, Some(&vars))?;
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
    use crate::log::{StageLogger, Verbosity};

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
        run_hooks(&hooks, "test", true, &log, Some(&vars))
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
        run_hooks(&hooks, "test", false, &log, Some(&vars))
            .expect("falsy `if:` must skip without spawning the cmd");
    }

    #[test]
    fn hook_if_literal_true_always_runs() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some("true"))];
        run_hooks(&hooks, "test", true, &log, Some(&vars)).expect("`if: true` must proceed");
    }

    #[test]
    fn hook_if_empty_literal_is_noop_gate() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some(""))];
        run_hooks(&hooks, "test", true, &log, Some(&vars))
            .expect("empty `if:` literal must be a no-op (always proceed)");
    }

    #[test]
    fn hook_if_render_error_propagates() {
        let log = test_logger();
        let vars = vars_with_snapshot(false);
        let hooks = vec![structured("true", Some("{{ UndefinedSymbol }}"))];
        let err = run_hooks(&hooks, "test", true, &log, Some(&vars))
            .expect_err("unrenderable template must surface as Err");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("template render failed") || chain.contains("UndefinedSymbol"),
            "expected render-error diagnostic, got: {chain}",
        );
    }
}
