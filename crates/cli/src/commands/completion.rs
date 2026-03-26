use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};
use std::io;

/// Generate shell completions and print them to stdout.
pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = crate::Cli::command();
    generate(shell, &mut cmd, "anodize", &mut io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_bash_produces_output() {
        // Generate bash completions into a buffer and verify non-empty output
        let mut cmd = crate::Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut cmd, "anodize", &mut buf);
        let output = String::from_utf8(buf).expect("completions should be valid UTF-8");
        assert!(!output.is_empty(), "bash completions should not be empty");
        assert!(
            output.contains("anodize"),
            "bash completions should reference the command name"
        );
    }

    #[test]
    fn test_completion_zsh_produces_output() {
        let mut cmd = crate::Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut cmd, "anodize", &mut buf);
        let output = String::from_utf8(buf).expect("completions should be valid UTF-8");
        assert!(!output.is_empty(), "zsh completions should not be empty");
    }

    #[test]
    fn test_completion_fish_produces_output() {
        let mut cmd = crate::Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Fish, &mut cmd, "anodize", &mut buf);
        let output = String::from_utf8(buf).expect("completions should be valid UTF-8");
        assert!(!output.is_empty(), "fish completions should not be empty");
    }

    #[test]
    fn test_completion_powershell_produces_output() {
        let mut cmd = crate::Cli::command();
        let mut buf = Vec::new();
        generate(Shell::PowerShell, &mut cmd, "anodize", &mut buf);
        let output = String::from_utf8(buf).expect("completions should be valid UTF-8");
        assert!(
            !output.is_empty(),
            "powershell completions should not be empty"
        );
    }
}
