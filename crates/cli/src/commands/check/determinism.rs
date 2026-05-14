//! Determinism-harness body.
//!
//! The flag set (`CheckDeterminismArgs`) lives in `anodizer_cli::lib` so it
//! participates in `Cli` parsing. The harness body — build the pipeline
//! twice from clean worktrees and byte-diff the resulting artifacts —
//! lands in a follow-up task; today this is a stub that bails so the
//! clap surface is exercisable.

use anodizer_cli::CheckDeterminismArgs;
use anyhow::Result;

pub fn run(_args: CheckDeterminismArgs) -> Result<()> {
    anyhow::bail!(
        "anodize check determinism is plumbed but the harness body lands in a follow-up task"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_determinism_stub_bails() {
        let args = CheckDeterminismArgs {
            runs: 2,
            stages: None,
            report: None,
            snapshot: false,
        };
        let result = run(args);
        assert!(result.is_err(), "determinism stub must bail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("plumbed") && msg.contains("follow-up"),
            "bail message should explain the stub status, got: {}",
            msg
        );
    }
}
