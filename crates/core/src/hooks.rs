use crate::config::HookEntry;
use crate::log::StageLogger;
use crate::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::process::Command;

/// Redact sensitive environment variable values from output strings.
///
/// Auto-discovers secret-looking env vars using the same heuristics as
/// GoReleaser's `redact.go`: key suffix matching and value prefix matching.
/// This catches both well-known and user-defined secrets.
fn redact_secrets(output: &str) -> String {
    let env: Vec<(String, String)> = std::env::vars().collect();
    crate::redact::redact_string(output, &env)
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
        let (raw_cmd, raw_dir, env, output_flag) = match hook {
            HookEntry::Simple(s) => (s.as_str(), None, None, None),
            HookEntry::Structured(h) => {
                (h.cmd.as_str(), h.dir.as_deref(), h.env.as_ref(), h.output)
            }
        };

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

        let expanded_env: Option<HashMap<String, String>> = match env {
            Some(envs) => {
                let mut out = HashMap::with_capacity(envs.len());
                for (k, v) in envs {
                    let expanded_v = if let Some(tv) = template_vars {
                        render_hook_template(v, tv, label)?
                    } else {
                        v.clone()
                    };
                    out.insert(k.clone(), expanded_v);
                }
                Some(out)
            }
            None => None,
        };

        if dry_run {
            log.status(&format!("[dry-run] {} hook: {}", label, cmd_str));
        } else {
            log.status(&format!("running {} hook: {}", label, cmd_str));
            let mut command = Command::new("sh");
            command.arg("-c").arg(&cmd_str);
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
