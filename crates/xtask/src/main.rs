use clap::{Parser, Subcommand};
use std::process;

mod gen_docs;

#[derive(Parser)]
#[command(name = "xtask", about = "Anodize development tasks")]
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
}

fn main() {
    let args = Xtask::parse();
    let result = match args.command {
        XtaskCommand::GenDocs { check } => gen_docs::run(check),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
