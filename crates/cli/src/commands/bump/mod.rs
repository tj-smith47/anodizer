//! `anodizer bump` — bump crate versions with Conventional-Commit inference.
//!
//! Scope (v1): see `.claude/plans/2026-04-18-bump-command.md`.

pub(crate) mod cargo_edit;
mod inference;
pub(crate) mod plan;

use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;

pub use plan::PlanRow;

pub struct BumpOpts {
    pub level_or_version: Option<String>,
    pub package: Vec<String>,
    pub workspace: bool,
    pub exclude: Vec<String>,
    pub pre: Option<String>,
    pub exact: bool,
    pub allow_dirty: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub commit: bool,
    pub sign: bool,
    pub commit_message: Option<String>,
    pub output: String,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    /// When true, refuse to bump any crate whose `crates[*].version` pin in
    /// `.anodizer.yaml` differs from the proposed next version. When false,
    /// the same condition only logs a warning.
    pub strict: bool,
}

pub fn run(opts: BumpOpts) -> Result<()> {
    let log = StageLogger::new(
        "bump",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    if opts.output != "text" && opts.output != "json" {
        bail!("--output must be 'text' or 'json', got '{}'", opts.output);
    }
    if opts.output == "json" && !opts.dry_run {
        bail!("--output json requires --dry-run");
    }

    // Dirty-tree guard (Phase 7 will flesh out the prompt; guard is cheap here).
    if !opts.allow_dirty && !opts.dry_run && anodizer_core::git::is_git_dirty() {
        bail!("working tree has uncommitted changes — commit them or pass --allow-dirty");
    }

    let workspace_root = discover_workspace_root(opts.config_override.as_deref())
        .context("could not locate workspace root (no Cargo.toml found)")?;

    let rows = plan::build_plan(&workspace_root, &opts).context("failed to build bump plan")?;

    if rows.is_empty() {
        log.status("nothing to bump");
        return Ok(());
    }

    // Enforce `.anodizer.yaml`'s `crates[*].version` pins. In strict mode this
    // is fatal; otherwise a warning. Runs BEFORE any output or prompt so the
    // user never confirms an invalid plan.
    enforce_version_pins(&workspace_root, &rows, &opts, &log)?;

    if opts.output == "json" {
        let json =
            serde_json::to_string_pretty(&rows).context("failed to serialize plan to JSON")?;
        println!("{}", json);
        return Ok(());
    }

    plan::render_text_table(&rows);

    if opts.dry_run {
        return Ok(());
    }

    if !opts.yes && is_interactive_stdout() {
        eprintln!("\nProceed? [y/N]");
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("failed to read confirmation")?;
        let a = answer.trim().to_ascii_lowercase();
        if a != "y" && a != "yes" {
            log.status("aborted");
            return Ok(());
        }
    }

    cargo_edit::apply_plan(&workspace_root, &rows, opts.exact, &log)?;

    if opts.commit {
        commit_plan(&workspace_root, &rows, &opts, &log)?;
    }

    log.status(&format!("bumped {} crate(s)", rows.len()));
    Ok(())
}

fn discover_workspace_root(config_override: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = config_override {
        // Config override points at .anodizer.yaml; walk up until we find Cargo.toml.
        if let Some(dir) = p.parent() {
            for ancestor in dir.ancestors() {
                if ancestor.join("Cargo.toml").is_file() {
                    return Ok(ancestor.to_path_buf());
                }
            }
        }
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!("no Cargo.toml found from {}", cwd.display());
}

fn is_interactive_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn commit_plan(
    workspace_root: &std::path::Path,
    rows: &[PlanRow],
    opts: &BumpOpts,
    log: &StageLogger,
) -> Result<()> {
    use std::process::Command;

    let mut staged: Vec<PathBuf> = Vec::new();
    for row in rows {
        for path in &row.edited_files {
            if !staged.contains(path) {
                staged.push(path.clone());
            }
        }
    }

    // Bundle changelog edits: for each non-skip row, ask stage-changelog to
    // render and persist the section for the crate's new version. Files are
    // written to disk here so they land in the same `git add` + `git commit`
    // as the Cargo.toml edits.
    for row in rows {
        if row.level == plan::BumpLevel::Skip {
            continue;
        }
        let crate_dir = match row.manifest.parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let tag_prefix = format!("{}-v", row.crate_name);
        let from_tag = inference::find_last_tag_for_prefix(workspace_root, &tag_prefix)?;
        let update = anodizer_stage_changelog::render_crate_section(
            workspace_root,
            &row.crate_name,
            &crate_dir,
            from_tag.as_deref(),
            &row.next,
        )
        .with_context(|| format!("failed to render changelog for {}", row.crate_name))?;
        let Some(update) = update else { continue };
        match update.insertion_mode {
            anodizer_stage_changelog::InsertionMode::Replace => {
                if let Some(parent) = update.file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                std::fs::write(&update.file_path, &update.rendered_text).with_context(|| {
                    format!(
                        "failed to write changelog at {}",
                        update.file_path.display()
                    )
                })?;
            }
        }
        if !staged.contains(&update.file_path) {
            staged.push(update.file_path);
        }
        log.verbose(&format!(
            "bundled changelog section for {} → {}",
            row.crate_name, row.next
        ));
    }

    // Cargo.lock update if present.
    let lockfile = workspace_root.join("Cargo.lock");
    if lockfile.is_file() {
        staged.push(lockfile);
    }

    for path in &staged {
        let rel = path.strip_prefix(workspace_root).unwrap_or(path.as_path());
        let out = Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .arg("add")
            .arg(rel)
            .output()
            .context("failed to invoke git add")?;
        if !out.status.success() {
            bail!(
                "git add {} failed: {}",
                rel.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    let message = opts
        .commit_message
        .clone()
        .unwrap_or_else(|| default_commit_message(rows));

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workspace_root).arg("commit");
    if opts.sign {
        cmd.arg("-S");
    }
    cmd.arg("-m").arg(&message);
    let out = cmd.output().context("failed to invoke git commit")?;
    if !out.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    log.verbose(&format!("created commit: {}", message));
    Ok(())
}

fn default_commit_message(rows: &[PlanRow]) -> String {
    if rows.len() == 1 {
        let r = &rows[0];
        format!("chore(release): bump {} → {}", r.crate_name, r.next)
    } else {
        let summary = rows
            .iter()
            .map(|r| format!("{} → {}", r.crate_name, r.next))
            .collect::<Vec<_>>()
            .join(", ");
        format!("chore(release): bump {}", summary)
    }
}

/// Validate the plan against `crates[*].version` pins in `.anodizer.yaml`.
/// In strict mode any pin mismatch is fatal; otherwise a warning is logged
/// and the bump proceeds.
fn enforce_version_pins(
    workspace_root: &std::path::Path,
    rows: &[PlanRow],
    opts: &BumpOpts,
    log: &StageLogger,
) -> Result<()> {
    let cfg_path = match opts.config_override.as_deref() {
        Some(p) => p.to_path_buf(),
        None => {
            let candidate = workspace_root.join(".anodizer.yaml");
            if !candidate.is_file() {
                return Ok(());
            }
            candidate
        }
    };
    if !cfg_path.is_file() {
        return Ok(());
    }
    let config = crate::pipeline::load_config(&cfg_path)
        .with_context(|| format!("failed to load {}", cfg_path.display()))?;
    let mut violations: Vec<String> = Vec::new();
    for row in rows {
        if row.level == plan::BumpLevel::Skip {
            continue;
        }
        let Some(crate_cfg) = config.crates.iter().find(|c| c.name == row.crate_name) else {
            continue;
        };
        let Some(pin) = crate_cfg.version.as_deref() else {
            continue;
        };
        if pin != row.next {
            violations.push(format!(
                "{}: configured version pin '{}' would be overwritten by proposed bump to '{}'",
                row.crate_name, pin, row.next
            ));
        }
    }
    if violations.is_empty() {
        return Ok(());
    }
    if opts.strict {
        bail!(
            "strict mode: refusing to bump pinned crate(s):\n  - {}",
            violations.join("\n  - ")
        );
    }
    for v in &violations {
        log.warn(&format!("version pin: {}", v));
    }
    Ok(())
}
