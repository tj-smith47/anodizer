//! `anodize check` parent subcommand body.
//!
//! The clap `CheckCmd` enum + `CheckDeterminismArgs` live in
//! `anodizer_cli::lib` (so they participate in `Cli` parsing). The bodies
//! that each variant dispatches to live in the submodules here:
//!
//! - `config` — validate `.anodizer.yaml` (the historic body of this
//!   command, now relocated under its own subcommand).
//! - `determinism` — run the determinism harness (stubbed; harness body
//!   lands in a follow-up task).

pub mod config;
pub mod determinism;
