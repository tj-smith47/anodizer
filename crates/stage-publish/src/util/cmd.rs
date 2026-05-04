//! Subprocess helper used across publisher git/PR/clone flows.

use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

/// Run a command in a specific working directory, failing with `label`
/// on spawn failure or non-zero exit.  Captures stdout/stderr so that
/// diagnostics are included in the error message.
pub(crate) fn run_cmd_in(dir: &Path, program: &str, args: &[&str], label: &str) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("{}: failed to run {} {}", label, program, args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}
