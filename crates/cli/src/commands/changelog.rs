use super::helpers;
use crate::commands::bump::cargo_edit::{WorkspaceInfo, load_workspace};
use crate::commands::bump::inference::find_last_tag_for_prefix;
use crate::commands::changelog_sync::{
    ChangelogRouting, RefreshTarget, extract_unreleased_section, refresh_changelogs,
};
use crate::commands::tag::{RepoShape, detect_repo_shape};
use crate::pipeline;
use anodizer_cli::ChangelogFormat;
use anodizer_core::config::Config;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Options for `anodizer changelog`.
///
/// Bundled struct rather than positional args so adding a flag at the CLI
/// layer doesn't ripple through every test. Mirrors the shape of
/// `release::ReleaseOpts` / `tag::TagOpts`.
pub struct ChangelogOpts {
    /// A specific crate in a workspace, or `None` for every selected crate.
    pub crate_name: Option<String>,
    /// Positional `[<tag>|<range>]`: a `from..to` range, a single `<tag>`
    /// (predecessor-resolved against the crate it belongs to), or `None` to
    /// refresh each crate's pending section against its last tag.
    pub range: Option<String>,
    /// Output format; defaults to keep-a-changelog.
    pub format: ChangelogFormat,
    /// Apply the regenerated `[Unreleased]` to the configured `CHANGELOG.md`
    /// file(s). Valid only with `--format keep-a-changelog`.
    pub write: bool,
    /// Preview as a snapshot release (forwarded to the release-notes path).
    pub snapshot: bool,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

/// The resolved commit range driving a changelog render.
struct ResolvedRange {
    from: Option<String>,
    to: Option<String>,
    /// When set, the positional was a single tag that resolved to exactly one
    /// crate; narrows the selection to that crate regardless of `--crate`.
    pinned_crate: Option<String>,
}

pub fn run(opts: ChangelogOpts) -> Result<()> {
    let ChangelogOpts {
        crate_name,
        range,
        format,
        write,
        snapshot,
        config_override,
        verbose,
        debug,
        quiet,
    } = opts;
    let log = StageLogger::new("changelog", Verbosity::from_flags(quiet, verbose, debug));

    // `--write` only makes sense for the in-place keep-a-changelog refresh;
    // release-notes / json stream to stdout for the user to redirect.
    if write && format != ChangelogFormat::KeepAChangelog {
        bail!(
            "--write is only valid with --format keep-a-changelog; \
             redirect stdout to capture release-notes/json output"
        );
    }

    let path = pipeline::find_config_with_logger(config_override.as_deref(), Some(&log))?;
    let config = pipeline::load_config(&path)?;

    let workspace_root = std::env::current_dir()?;

    match format {
        ChangelogFormat::KeepAChangelog => run_refresh(
            &workspace_root,
            &config,
            crate_name.as_deref(),
            range.as_deref(),
            write,
            &log,
        ),
        ChangelogFormat::ReleaseNotes => run_release_notes(
            &workspace_root,
            config,
            crate_name,
            range.as_deref(),
            snapshot,
            verbose,
            debug,
            &log,
        ),
        ChangelogFormat::Json => run_json(
            &workspace_root,
            &config,
            crate_name.as_deref(),
            range.as_deref(),
            &log,
        ),
    }
}

/// Resolve the positional `[<tag>|<range>]` into a concrete `(from, to)` plus an
/// optional crate pin.
///
/// - `None` → unbounded `(None, None)`; each target resolves its own last tag.
/// - `a..b` → explicit `(Some(a), Some(b))`, no pin (applies to all targets).
/// - `<tag>` → resolve the owning crate from its tag prefix, pin to it, and set
///   `from` to the predecessor tag (the tag immediately below `<tag>` in that
///   crate's semver-sorted list; `None` when `<tag>` is the earliest) and `to`
///   to `<tag>`.
fn resolve_range(
    workspace_root: &Path,
    config: &Config,
    range: Option<&str>,
) -> Result<ResolvedRange> {
    let Some(spec) = range else {
        return Ok(ResolvedRange {
            from: None,
            to: None,
            pinned_crate: None,
        });
    };
    if let Some((from, to)) = spec.split_once("..") {
        return Ok(ResolvedRange {
            from: (!from.is_empty()).then(|| from.to_string()),
            to: (!to.is_empty()).then(|| to.to_string()),
            pinned_crate: None,
        });
    }
    // Single tag: resolve the owning crate + predecessor.
    let (owning_crate, prefix) = resolve_tag_owner(config, spec)?;
    let predecessor = predecessor_tag(workspace_root, &prefix, spec)?;
    Ok(ResolvedRange {
        from: predecessor,
        to: Some(spec.to_string()),
        pinned_crate: Some(owning_crate),
    })
}

/// Resolve which crate a single tag belongs to from its tag-template prefix,
/// returning `(crate_name, prefix)`. Mirrors the `resolve-tag` command:
/// longest-matching prefix wins, and the remainder must look like a version.
fn resolve_tag_owner(config: &Config, tag: &str) -> Result<(String, String)> {
    let all_crates = config.crates.iter().chain(
        config
            .workspaces
            .as_deref()
            .unwrap_or_default()
            .iter()
            .flat_map(|w| &w.crates),
    );
    let mut best: Option<(&str, String)> = None;
    for c in all_crates {
        let prefix =
            git::extract_tag_prefix(&c.tag_template).unwrap_or_else(|| format!("{}-v", c.name));
        if let Some(remainder) = tag.strip_prefix(&prefix) {
            let is_version = remainder
                .split('.')
                .next()
                .is_some_and(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit()));
            if is_version && best.as_ref().is_none_or(|(_, p)| prefix.len() > p.len()) {
                best = Some((c.name.as_str(), prefix));
            }
        }
    }
    match best {
        Some((name, prefix)) => Ok((name.to_string(), prefix)),
        None => bail!("no crate in the config matches tag '{}'", tag),
    }
}

/// The tag immediately preceding `tag` in the prefix's semver-sorted list, or
/// `None` when `tag` is the earliest (or absent from the list).
///
/// `get_all_semver_tags_in` returns matches descending by version, so the
/// predecessor is the first entry strictly below `tag`.
fn predecessor_tag(workspace_root: &Path, prefix: &str, tag: &str) -> Result<Option<String>> {
    let tags = git::get_all_semver_tags_in(workspace_root, prefix, None, None)
        .with_context(|| format!("list tags with prefix '{}'", prefix))?;
    let target = git::parse_semver_tag(tag).ok();
    let Some(target) = target else {
        return Ok(None);
    };
    for candidate in &tags {
        if let Ok(v) = git::parse_semver_tag(candidate)
            && v < target
        {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// Enumerate the crates selected for rendering across all three config modes,
/// honoring `--crate` and a single-tag crate pin.
///
/// Each entry is `(crate_name, crate_dir, tag_prefix)`. For lockstep the sole
/// entry is the workspace root aggregate, prefixed by the resolved
/// `tag.tag_prefix` (default `v`) — identical to how `tag`/`bump` bound the
/// lockstep range.
fn select_crates(
    workspace_root: &Path,
    config: &Config,
    workspace: Option<&WorkspaceInfo>,
    crate_filter: Option<&str>,
) -> Vec<(String, PathBuf, String)> {
    let prefix_for = |c: &anodizer_core::config::CrateConfig| -> String {
        git::extract_tag_prefix(&c.tag_template).unwrap_or_else(|| format!("{}-v", c.name))
    };
    // The global tag prefix that `tag`/`bump` apply to the lockstep / single
    // unit: the configured `tag.tag_prefix`, defaulting to `v`. Resolving it
    // here keeps the lockstep refresh range aligned with the tags `tag`
    // actually creates (e.g. `release-v0.1.0`); a hardcoded `v` would miss them
    // and silently degrade the range to full history.
    let global_prefix = config
        .tag
        .as_ref()
        .and_then(|t| t.tag_prefix.clone())
        .unwrap_or_else(|| "v".to_string());
    let entries: Vec<(String, PathBuf, String)> = match detect_repo_shape(Some(config), workspace) {
        RepoShape::Single => {
            // The sole crate (or a config-less single crate): one
            // global-prefixed target at its directory (workspace root when no
            // crate is defined). A crate's own `tag_template` still wins when
            // it sets one; otherwise it inherits the global prefix.
            let c = config.crates.first().or_else(|| {
                config
                    .workspaces
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .flat_map(|w| &w.crates)
                    .next()
            });
            match c {
                Some(c) => vec![(
                    c.name.clone(),
                    workspace_root.join(&c.path),
                    git::extract_tag_prefix(&c.tag_template)
                        .unwrap_or_else(|| global_prefix.clone()),
                )],
                None => vec![(
                    config.project_name.clone(),
                    workspace_root.to_path_buf(),
                    global_prefix.clone(),
                )],
            }
        }
        RepoShape::Lockstep => {
            // One aggregate target at the workspace root, using the resolved
            // global `tag.tag_prefix`.
            vec![(
                config.project_name.clone(),
                workspace_root.to_path_buf(),
                global_prefix.clone(),
            )]
        }
        RepoShape::PerCrate(groups) => groups
            .iter()
            .flatten()
            .map(|c| (c.name.clone(), workspace_root.join(&c.path), prefix_for(c)))
            .collect(),
    };

    match crate_filter {
        Some(name) => entries.into_iter().filter(|(n, _, _)| n == name).collect(),
        None => entries,
    }
}

/// keep-a-changelog: refresh each selected crate's pending `[Unreleased]`
/// section. Previews to stdout unless `write` writes the file(s) in place.
fn run_refresh(
    workspace_root: &Path,
    config: &Config,
    crate_filter: Option<&str>,
    range: Option<&str>,
    write: bool,
    log: &StageLogger,
) -> Result<()> {
    let resolved = resolve_range(workspace_root, config, range)?;
    let effective_filter = resolved.pinned_crate.as_deref().or(crate_filter);
    let workspace = load_workspace(workspace_root).ok();
    let selected = select_crates(workspace_root, config, workspace.as_ref(), effective_filter);
    if selected.is_empty() {
        log.warn("no crates selected for changelog refresh");
        return Ok(());
    }

    let targets: Vec<RefreshTarget> = selected
        .into_iter()
        .map(|(name, dir, prefix)| {
            // An explicit range drives every target; otherwise each target
            // resolves its own last matching tag as the lower bound.
            let from_tag = match resolved.from.clone() {
                Some(f) => Some(f),
                None if resolved.to.is_none() => find_last_tag_for_prefix(workspace_root, &prefix)?,
                None => None,
            };
            Ok(RefreshTarget {
                crate_name: name,
                crate_dir: dir,
                from_tag,
                to_ref: resolved.to.clone(),
            })
        })
        .collect::<Result<_>>()?;

    let empty = anodizer_core::config::ChangelogConfig::default();
    let routing = ChangelogRouting::from_config(config.changelog.as_ref().unwrap_or(&empty));
    let outputs = refresh_changelogs(workspace_root, &targets, &routing, write, log)?;

    if outputs.is_empty() {
        log.warn("no changelog sections to refresh");
        return Ok(());
    }

    if write {
        // Files written + logged by refresh_changelogs; nothing to print.
        return Ok(());
    }

    // Preview: print only the regenerated `[Unreleased]` section of each file,
    // attributing multi-file output with a `--- <path> ---` header.
    let multi = outputs.len() > 1;
    let mut stdout = std::io::stdout();
    for out in &outputs {
        let section = extract_unreleased_section(&out.rendered_text);
        if multi {
            writeln!(stdout, "--- {} ---", out.rel_path)
                .context("changelog: write preview separator")?;
        }
        writeln!(stdout, "{}", section).context("changelog: write preview section")?;
        if multi {
            writeln!(stdout).context("changelog: write preview spacer")?;
        }
    }
    Ok(())
}

/// release-notes: the historical grouped-bullet GitHub-body markdown to stdout,
/// driven by the resolved range. Honors `--crate` and `--snapshot`.
#[allow(clippy::too_many_arguments)]
fn run_release_notes(
    workspace_root: &Path,
    mut config: Config,
    crate_filter: Option<String>,
    range: Option<&str>,
    snapshot: bool,
    verbose: bool,
    debug: bool,
    log: &StageLogger,
) -> Result<()> {
    let resolved = resolve_range(workspace_root, &config, range)?;
    let effective_filter = resolved.pinned_crate.clone().or(crate_filter);

    log.status("generating release notes");

    let selected_crates: Vec<String> = match effective_filter.as_ref() {
        Some(name) => vec![name.clone()],
        None => Vec::new(),
    };

    // Apply the workspace overlay when the filter resolves to a workspace crate
    // so monorepo configs (top-level `workspaces:` rather than `crates:`) hand
    // the changelog stage the right per-crate context.
    if let Some(ref target) = effective_filter
        && config.crates.is_empty()
    {
        let ws_for_target = config
            .workspaces
            .as_ref()
            .and_then(|ws_list| {
                ws_list
                    .iter()
                    .find(|ws| ws.crates.iter().any(|c| &c.name == target))
            })
            .cloned();
        if let Some(ws) = ws_for_target {
            log.verbose(&format!(
                "--crate {} lives in workspace '{}'; applying workspace overlay",
                target, ws.name
            ));
            helpers::apply_workspace_overlay(&mut config, &ws);
        }
    }

    let ctx_opts = ContextOptions {
        verbose,
        debug,
        selected_crates,
        snapshot,
        // The range start, carried as a dedicated option so the changelog stage
        // overrides its auto-discovered previous tag only when the user supplied
        // a range (the always-auto-populated `PreviousTag` template var cannot
        // signal "user asked for this").
        changelog_from: resolved.from.clone(),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    helpers::resolve_git_context(&mut ctx, &config, log)?;

    // Apply range overrides AFTER `resolve_git_context` has filled the default
    // `Tag` / `PreviousTag` so the user override wins. `to` becomes `Tag` (the
    // upper bound the stage reads directly); `from` sets `PreviousTag` so
    // header/footer templates see it (the range-start driving commit collection
    // flows via `ctx_opts.changelog_from`).
    if let Some(ref t) = resolved.to {
        ctx.template_vars_mut().set("Tag", t);
    }
    if let Some(ref f) = resolved.from {
        ctx.template_vars_mut().set("PreviousTag", f);
    }

    let stage = anodizer_stage_changelog::ChangelogStage;
    stage.run(&mut ctx)?;

    // Stable iteration order (HashMap iteration is non-deterministic, which
    // makes stdout flicker across runs and breaks test pinning).
    let mut entries: Vec<(&String, &String)> = ctx.stage_outputs.changelogs.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut aggregated = String::new();
    for (name, body) in &entries {
        if entries.len() > 1 {
            // Multi-crate separator keeps each crate's body attributable.
            aggregated.push_str(&format!("\n---\n{}\n---\n\n", name));
        }
        aggregated.push_str(body);
        if !body.ends_with('\n') {
            aggregated.push('\n');
        }
    }

    print!("{}", aggregated);

    if ctx.stage_outputs.changelogs.is_empty() {
        log.warn("no changelogs generated");
    }
    Ok(())
}

/// json: a stable JSON array of `{ crate, from, to, groups }` objects, one per
/// selected crate, sorted by crate name. Respects the resolved range.
fn run_json(
    workspace_root: &Path,
    config: &Config,
    crate_filter: Option<&str>,
    range: Option<&str>,
    log: &StageLogger,
) -> Result<()> {
    let resolved = resolve_range(workspace_root, config, range)?;
    let effective_filter = resolved.pinned_crate.as_deref().or(crate_filter);
    let workspace = load_workspace(workspace_root).ok();
    let selected = select_crates(workspace_root, config, workspace.as_ref(), effective_filter);
    if selected.is_empty() {
        log.warn("no crates selected for changelog json");
    }

    let mut elems: Vec<serde_json::Value> = Vec::new();
    for (name, dir, prefix) in &selected {
        let from_tag = match resolved.from.clone() {
            Some(f) => Some(f),
            None if resolved.to.is_none() => find_last_tag_for_prefix(workspace_root, prefix)?,
            None => None,
        };
        let json = anodizer_stage_changelog::render_changelog_json(
            workspace_root,
            dir,
            from_tag.as_deref(),
            resolved.to.as_deref(),
        )
        .with_context(|| format!("render json changelog for {}", name))?;
        // `None` (no config / no commits) still contributes a stable entry so
        // the array shape is predictable; lift its payload onto a `crate` field.
        let payload: serde_json::Value = match json {
            Some(s) => serde_json::from_str(&s)
                .with_context(|| format!("parse json changelog for {}", name))?,
            None => serde_json::json!({
                "from": from_tag,
                "to": resolved.to.clone().unwrap_or_else(|| "HEAD".to_string()),
                "groups": [],
            }),
        };
        let mut obj = serde_json::Map::new();
        obj.insert("crate".to_string(), serde_json::Value::String(name.clone()));
        if let serde_json::Value::Object(map) = payload {
            for (k, v) in map {
                obj.insert(k, v);
            }
        }
        elems.push(serde_json::Value::Object(obj));
    }
    elems.sort_by(|a, b| {
        a.get("crate")
            .and_then(|v| v.as_str())
            .cmp(&b.get("crate").and_then(|v| v.as_str()))
    });

    let out = serde_json::to_string_pretty(&serde_json::Value::Array(elems))
        .context("serialize changelog json array")?;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", out).context("changelog: write json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::CrateConfig;
    use serial_test::serial;

    fn default_opts(config: Option<&Path>) -> ChangelogOpts {
        ChangelogOpts {
            crate_name: None,
            range: None,
            format: ChangelogFormat::default(),
            write: false,
            snapshot: false,
            config_override: config.map(|p| p.to_path_buf()),
            verbose: false,
            debug: false,
            quiet: true,
        }
    }

    fn crate_cfg(name: &str, tag_template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            tag_template: tag_template.to_string(),
            ..Default::default()
        }
    }

    fn cfg_with_crates(crates: Vec<CrateConfig>) -> Config {
        Config {
            crates,
            ..Default::default()
        }
    }

    #[test]
    fn default_format_is_keep_a_changelog() {
        assert_eq!(ChangelogFormat::default(), ChangelogFormat::KeepAChangelog);
    }

    #[test]
    #[serial]
    fn missing_config_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let err = run(default_opts(Some(&bogus))).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");
    }

    #[test]
    #[serial]
    fn write_with_release_notes_format_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let mut opts = default_opts(Some(&bogus));
        opts.write = true;
        opts.format = ChangelogFormat::ReleaseNotes;
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("--write is only valid"), "{err}");
    }

    #[test]
    #[serial]
    fn write_with_json_format_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let mut opts = default_opts(Some(&bogus));
        opts.write = true;
        opts.format = ChangelogFormat::Json;
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("--write is only valid"), "{err}");
    }

    /// The `--write` guard fires before config loading, so a missing config
    /// never masks the format error.
    #[test]
    #[serial]
    fn write_guard_precedes_config_load() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("missing.yaml");
        let mut opts = default_opts(Some(&bogus));
        opts.write = true;
        opts.format = ChangelogFormat::Json;
        // Even with a bogus config path, the format/write conflict wins.
        let err = run(opts).unwrap_err().to_string();
        assert!(err.contains("--write"), "{err}");
    }

    #[test]
    fn resolve_range_none_is_unbounded() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, None).unwrap();
        assert!(r.from.is_none());
        assert!(r.to.is_none());
        assert!(r.pinned_crate.is_none());
    }

    #[test]
    fn resolve_range_explicit_range_splits() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, Some("v0.1.0..v0.2.0")).unwrap();
        assert_eq!(r.from.as_deref(), Some("v0.1.0"));
        assert_eq!(r.to.as_deref(), Some("v0.2.0"));
        assert!(r.pinned_crate.is_none());
    }

    #[test]
    fn resolve_range_open_ended_range_left() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, Some("..v0.2.0")).unwrap();
        assert!(r.from.is_none());
        assert_eq!(r.to.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn resolve_tag_owner_picks_longest_prefix() {
        let cfg = cfg_with_crates(vec![
            crate_cfg("app", "v{{ .Version }}"),
            crate_cfg("core", "core-v{{ .Version }}"),
        ]);
        let (name, prefix) = resolve_tag_owner(&cfg, "core-v0.2.0").unwrap();
        assert_eq!(name, "core");
        assert_eq!(prefix, "core-v");
        let (name, prefix) = resolve_tag_owner(&cfg, "v1.0.0").unwrap();
        assert_eq!(name, "app");
        assert_eq!(prefix, "v");
    }

    #[test]
    fn resolve_tag_owner_no_match_errors() {
        let cfg = cfg_with_crates(vec![crate_cfg("core", "core-v{{ .Version }}")]);
        let err = resolve_tag_owner(&cfg, "other-v1.0.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no crate"), "{err}");
    }

    #[test]
    fn select_crates_per_crate_filters() {
        let cfg = cfg_with_crates(vec![
            crate_cfg("app", "v{{ .Version }}"),
            crate_cfg("core", "core-v{{ .Version }}"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let all = select_crates(tmp.path(), &cfg, None, None);
        assert_eq!(all.len(), 2);
        let filtered = select_crates(tmp.path(), &cfg, None, Some("core"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "core");
        assert_eq!(filtered[0].2, "core-v");
    }
}
