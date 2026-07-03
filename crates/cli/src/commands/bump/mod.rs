//! `anodizer bump` — bump crate versions with Conventional-Commit inference.

pub(crate) mod cargo_edit;
pub(crate) mod inference;
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
    /// Refresh `CHANGELOG.md` in the bump commit. Opt-in and only consulted under
    /// `--commit` (the changelog gate runs inside the commit path).
    pub changelog: bool,
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

    // `--changelog` only takes effect inside the `--commit` path (the changelog
    // refresh rides in the bump commit). Reject the combination loudly rather
    // than silently ignoring `--changelog`, mirroring the help text
    // ("requires --commit") and `tag rollback`'s hard-fail on a misplaced flag.
    if opts.changelog && !opts.commit {
        bail!("--changelog requires --commit (the changelog refresh rides in the bump commit)");
    }
    if opts.output == "json" && !opts.dry_run {
        bail!("--output json requires --dry-run");
    }

    // Dirty-tree guard.
    if !opts.allow_dirty && !opts.dry_run && anodizer_core::git::is_git_dirty() {
        bail!("working tree has uncommitted changes — commit them or pass --allow-dirty");
    }

    let workspace_root =
        crate::commands::helpers::discover_workspace_root(opts.config_override.as_deref())
            .context("could not locate workspace root (no Cargo.toml found)")?;

    // The command's ONLY config load — threaded into the coherence guard,
    // the plan builder, and the pin enforcement below. Loading per consumer
    // would re-emit the load-time legacy-alias warnings once per load in a
    // single invocation.
    let bump_config = load_bump_config(&workspace_root, &opts, &log)?;
    // Submitter moderation-queue advisories are verbose-only; emit them once
    // off this single load (hidden at the default log level).
    if let Some(ref cfg) = bump_config {
        crate::pipeline::emit_config_advisories(cfg, &log);
    }

    // Reject an incoherent flat-aggregate config (members sharing one tag prefix
    // but disagreeing on `[package].version`) before any work, identically to
    // `tag` and `changelog`.
    let bump_workspace = cargo_edit::load_workspace(&workspace_root)?;
    crate::commands::tag::guard_flat_aggregate_coherence(
        bump_config.as_ref(),
        bump_workspace.as_ref(),
        &workspace_root,
    )?;

    let rows = plan::build_plan(&workspace_root, &opts, bump_config.as_ref())
        .context("failed to build bump plan")?;

    if rows.is_empty() {
        log.status("nothing to bump");
        return Ok(());
    }

    // Enforce `.anodizer.yaml`'s `crates[*].version` pins. In strict mode this
    // is fatal; otherwise a warning. Runs BEFORE any output or prompt so the
    // user never confirms an invalid plan.
    enforce_version_pins(bump_config.as_ref(), &rows, &opts, &log)?;

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
        log.status("\nProceed? [y/N]");
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
        commit_plan(
            &workspace_root,
            &rows,
            &opts,
            bump_config.as_ref(),
            bump_workspace.as_ref(),
            &log,
        )?;
    }

    log.status(&format!("bumped {} crate(s)", rows.len()));
    Ok(())
}

fn is_interactive_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn commit_plan(
    workspace_root: &std::path::Path,
    rows: &[PlanRow],
    opts: &BumpOpts,
    changelog_config: Option<&anodizer_core::config::Config>,
    workspace: Option<&cargo_edit::WorkspaceInfo>,
    log: &StageLogger,
) -> Result<()> {
    let mut staged: Vec<PathBuf> = Vec::new();
    for row in rows {
        for path in &row.edited_files {
            if !staged.contains(path) {
                staged.push(path.clone());
            }
        }
    }

    // Bundle changelog edits: render + persist each non-skip crate's section
    // for its new version so the files land in the same `git add` + `git commit`
    // as the Cargo.toml edits. The previous tag bounds each crate's commit range.
    //
    // Gated by the shared opt-in + `changelog:`-presence + `skip:` resolution so
    // `bump` and `tag` honor `--changelog` and `changelog: { skip: true }`
    // identically. The refresh runs only under `--changelog` (opt-in).
    //
    // `changelog_config` / `workspace` are threaded in from `run` (loaded once
    // there for the coherence guard) so a single `bump` parses the config and
    // Cargo workspace exactly once, not 2–3×.
    let changelog_enabled = crate::commands::changelog_sync::resolve_changelog_enabled(
        changelog_config,
        opts.changelog,
    );
    let mut changelog_targets: Vec<crate::commands::changelog_sync::ChangelogTarget> = Vec::new();
    for row in rows {
        if !changelog_enabled || row.level == plan::BumpLevel::Skip {
            continue;
        }
        let crate_dir = match row.manifest.parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        // Resolve the crate's tag prefix from its configured `tag_template`
        // (the same shared resolution the version-inference path uses), so
        // the previous-tag range and the promoted heading honor a
        // `v{{ Version }}` / custom scheme.
        let tag_template = changelog_config
            .and_then(|cfg| plan::find_crate_in_config(cfg, &row.crate_name))
            .map(|c| c.tag_template.as_str())
            .unwrap_or("");
        let tag_prefix = anodizer_core::git::per_crate_tag_prefix(&row.crate_name, tag_template);
        let from_tag = inference::find_last_tag_for_prefix(workspace_root, &tag_prefix)?;
        let full_tag = format!("{}{}", tag_prefix, row.next);
        changelog_targets.push(crate::commands::changelog_sync::ChangelogTarget {
            crate_name: row.crate_name.clone(),
            crate_dir,
            from_tag,
            to_version: row.next.clone(),
            full_tag,
        });
    }
    let empty_changelog_config = anodizer_core::config::ChangelogConfig::default();
    let mut routing = crate::commands::changelog_sync::ChangelogRouting::from_config(
        changelog_config
            .and_then(|c| c.changelog.as_ref())
            .unwrap_or(&empty_changelog_config),
    );

    // Collapse same-prefix shared-root targets to ONE flat aggregate (mirroring
    // `tag --changelog` and the `changelog` command): a flat `crates:` list
    // sharing one tag track and routing to one shared root is a single lockstep
    // release, not N multi-track subsections. Promoting each member under the
    // same `## [v<X.Y.Z>]` heading would strand every member after the first and
    // graft spurious `### <crate>` subsections.
    //
    // The flat-aggregate DECISION is the shared shape classification
    // (`detect_repo_shape` → `FlatAggregate`), not a local prefix re-derivation,
    // so it can't drift from `tag`/`changelog`. The routing gate
    // (`root_enabled && !per_crate`) still applies: per-crate files keep their
    // per-crate sections.
    if changelog_enabled
        && changelog_targets.len() > 1
        && routing.root_enabled
        && !routing.per_crate
        && let Some(cfg) = changelog_config
    {
        let is_flat_aggregate = matches!(
            crate::commands::tag::detect_repo_shape(workspace_root, Some(cfg), workspace),
            crate::commands::tag::RepoShape::FlatAggregate(_)
        );
        if is_flat_aggregate && let Some(first) = changelog_targets.first().cloned() {
            changelog_targets = vec![crate::commands::changelog_sync::ChangelogTarget {
                crate_name: cfg.project_name.clone(),
                crate_dir: workspace_root.to_path_buf(),
                from_tag: first.from_tag,
                to_version: first.to_version,
                full_tag: first.full_tag,
            }];
            routing.single_track = true;
        }
    }

    // Genuine multi-track root: thread the topology signal + the full
    // root-routed crate-name set so the root renderer bootstraps every crate's
    // subsection (no last-writer-wins on a fresh root). The crate-name set is the
    // FULL configured set (one shared `config_root_crate_names` source) so the
    // classification fallback never sees a changed-crates-only subset; the
    // multitrack COUNT is the number of bumped tracks routed to the root.
    let bumped_root_tracks = changelog_targets
        .iter()
        .filter(|t| {
            crate::commands::changelog_sync::crate_in_root(&t.crate_name, routing.root_crates)
        })
        .count();
    routing.root_crate_names = changelog_config
        .map(|cfg| {
            crate::commands::changelog_sync::config_root_crate_names(cfg, routing.root_crates)
        })
        .unwrap_or_default();
    routing.multitrack = routing.root_enabled && !routing.single_track && bumped_root_tracks > 1;

    let changelog_paths = crate::commands::changelog_sync::render_and_stage_changelogs(
        workspace_root,
        &changelog_targets,
        &routing,
        false,
        log,
    )?;
    for rel in changelog_paths {
        let path = workspace_root.join(&rel);
        if !staged.contains(&path) {
            staged.push(path);
        }
    }

    // Cargo.lock update if present.
    let lockfile = workspace_root.join("Cargo.lock");
    if lockfile.is_file() {
        staged.push(lockfile);
    }

    for path in &staged {
        let rel = path.strip_prefix(workspace_root).unwrap_or(path.as_path());
        anodizer_core::git::add_path_in(workspace_root, rel)?;
    }

    let message = opts
        .commit_message
        .clone()
        .unwrap_or_else(|| default_commit_message(rows));

    anodizer_core::git::commit_in(workspace_root, &message, opts.sign)?;
    log.verbose(&format!("created commit \"{}\"", message));
    Ok(())
}

fn default_commit_message(rows: &[PlanRow]) -> String {
    let summary = if rows.len() == 1 {
        let r = &rows[0];
        format!("{} → {}", r.crate_name, r.next)
    } else {
        rows.iter()
            .map(|r| format!("{} → {}", r.crate_name, r.next))
            .collect::<Vec<_>>()
            .join(", ")
    };
    anodizer_core::git::release_bump_subject(&summary, "")
}

/// Load the anodizer config once for the whole bump run (`--config`
/// override, else the shared well-known-name discovery
/// ([`crate::pipeline::find_config_in`]) rooted at `workspace_root` — the
/// same candidate set every other command honors, including the Cargo.toml
/// defaults fallback). The single result is shared by the coherence guard,
/// the plan builder's `tag_template` lookup, the pin enforcement, and the
/// changelog gate — each consumer loading separately would re-emit every
/// static-config warning per load.
///
/// `Ok(None)` when no config (and no Cargo.toml fallback) exists — every
/// consumer degrades to its no-config behavior. The Cargo.toml defaults
/// fallback warns through `log` (the same signal release/tag emit for the
/// same event). An explicit `--config` pointing at a missing file, or a
/// file that exists but fails to load, is a hard error: pin enforcement is
/// a correctness gate and must not be silently skipped.
fn load_bump_config(
    workspace_root: &std::path::Path,
    opts: &BumpOpts,
    log: &StageLogger,
) -> Result<Option<anodizer_core::config::Config>> {
    let cfg_path = match opts.config_override.as_deref() {
        Some(p) => {
            if !p.is_file() {
                bail!("config file not found: {}", p.display());
            }
            p.to_path_buf()
        }
        None => match crate::pipeline::find_config_in(workspace_root) {
            Ok(p) => {
                if p.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
                    log.warn(crate::pipeline::CARGO_TOML_FALLBACK_WARNING);
                }
                p
            }
            Err(_) => return Ok(None),
        },
    };
    crate::pipeline::load_config(&cfg_path)
        .map(Some)
        .with_context(|| format!("failed to load {}", cfg_path.display()))
}

/// Validate the plan against `crates[*].version` pins in `.anodizer.yaml`.
/// In strict mode any pin mismatch is fatal; otherwise a warning is logged
/// and the bump proceeds. `config` is the run's single shared load
/// ([`load_bump_config`]); `None` (no config file) means no pins to enforce.
fn enforce_version_pins(
    config: Option<&anodizer_core::config::Config>,
    rows: &[PlanRow],
    opts: &BumpOpts,
    log: &StageLogger,
) -> Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    let mut violations: Vec<String> = Vec::new();
    for row in rows {
        if row.level == plan::BumpLevel::Skip {
            continue;
        }
        let Some(crate_cfg) = config.find_crate(&row.crate_name) else {
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
        log.warn(&format!("version pin violation — {}", v));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BumpOpts {
        BumpOpts {
            level_or_version: Some("patch".to_string()),
            package: vec![],
            workspace: false,
            exclude: vec![],
            pre: None,
            exact: false,
            allow_dirty: true,
            yes: true,
            dry_run: true,
            commit: false,
            changelog: false,
            sign: false,
            commit_message: None,
            output: "text".to_string(),
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            strict: false,
        }
    }

    #[test]
    fn changelog_without_commit_is_rejected() {
        let mut o = opts();
        o.changelog = true;
        o.commit = false;
        let err = run(o).unwrap_err();
        assert!(
            err.to_string().contains("--changelog requires --commit"),
            "expected the changelog/commit guard to fire, got: {err}"
        );
    }

    /// `--config <missing file>` must bail loudly, never silently proceed
    /// with defaults — the same guard every sibling command pins.
    #[test]
    #[serial_test::serial]
    fn missing_config_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.config_override = Some(tmp.path().join("nope.yaml"));
        let err = run(o).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");
    }
}
