use crate::config::HookEntry;
use crate::log::StageLogger;
use crate::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::process::Command;

/// Redact known sensitive environment variable values from output strings.
///
/// Replaces the runtime value of each known secret env var with `***` so that
/// hook stdout/stderr never leaks credentials to logs.
fn redact_secrets(output: &str) -> String {
    let mut result = output.to_string();
    let secret_vars = [
        "ANODIZE_GITHUB_TOKEN",
        "GITHUB_TOKEN",
        "GITLAB_TOKEN",
        "GITEA_TOKEN",
        "DISCORD_WEBHOOK_TOKEN",
        "TELEGRAM_TOKEN",
        "SLACK_WEBHOOK",
        "COSIGN_PASSWORD",
        "GPG_PASSPHRASE",
        "NFPM_PASSPHRASE",
    ];
    for var in &secret_vars {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            result = result.replace(&val, "***");
        }
    }
    result
}

/// Render a hook template string through the full Tera engine.
///
/// This gives hooks the same template capabilities as all other config fields:
/// conditionals, filters, `{{ .Env.VAR }}`, etc.  Falls back to the raw
/// string if rendering fails (e.g. the string contains no template syntax).
fn render_hook_template(template: &str, vars: &TemplateVars) -> String {
    template::render(template, vars).unwrap_or_else(|_| template.to_string())
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

        // Template-expand the command string through Tera
        let cmd_str = if let Some(tv) = template_vars {
            render_hook_template(raw_cmd, tv)
        } else {
            raw_cmd.to_string()
        };

        // Template-expand the working directory
        let dir_str = raw_dir.map(|d| {
            if let Some(tv) = template_vars {
                render_hook_template(d, tv)
            } else {
                d.to_string()
            }
        });

        // Template-expand environment variable values
        let expanded_env: Option<HashMap<String, String>> = env.map(|envs| {
            envs.iter()
                .map(|(k, v)| {
                    let expanded_v = if let Some(tv) = template_vars {
                        render_hook_template(v, tv)
                    } else {
                        v.clone()
                    };
                    (k.clone(), expanded_v)
                })
                .collect()
        });

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
