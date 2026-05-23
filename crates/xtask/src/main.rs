use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

mod gen_docs;
mod validate_readme;

#[derive(Parser)]
#[command(name = "xtask", about = "Anodizer development tasks")]
struct Xtask {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Subcommand)]
enum XtaskCommand {
    /// Generate CLI and configuration reference documentation
    GenDocs {
        /// Check if generated docs are up-to-date (exit 1 if stale)
        #[arg(long)]
        check: bool,
    },
    /// Validate every anodizer YAML config block in README.md
    ValidateReadme {
        /// Path to the README to validate (default: README.md in workspace root)
        #[arg(long)]
        readme: Option<PathBuf>,
    },
}

fn main() {
    let args = Xtask::parse();
    let result = match args.command {
        XtaskCommand::GenDocs { check } => gen_docs::run(check),
        XtaskCommand::ValidateReadme { readme } => validate_readme::run(readme.as_deref()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
