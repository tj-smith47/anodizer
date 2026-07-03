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

/// The lower bound of a changelog range.
///
/// Three states because an omitted arg and an explicit empty lower bound must
/// mean different things: omitting the positional refreshes each target's
/// pending window (since its last release), while writing `..` (an explicit
/// empty left side) asks for full history. Collapsing both to "no lower
/// bound" would make `..` indistinguishable from omission — the trap this
/// enum exists to remove.
enum RangeStart {
    /// Arg omitted: each target resolves its own last release tag (the pending
    /// `[Unreleased]` window).
    Pending,
    /// Explicit empty lower bound (`..`, `..<ref>`): no lower bound — full history.
    FullHistory,
    /// Explicit ref (`<ref>..`, `<ref>..<ref>`, or a single tag's predecessor).
    Ref(String),
}

/// The resolved commit range driving a changelog render.
struct ResolvedRange {
    start: RangeStart,
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
    let config = pipeline::load_config_logged(&path, &log)?;

    let workspace_root =
        crate::commands::helpers::discover_workspace_root(config_override.as_deref())?;

    // Reject an incoherent flat-aggregate config (members sharing one tag prefix
    // but disagreeing on `[package].version`) before any work, identically to
    // `tag` and `bump`.
    let workspace = load_workspace(&workspace_root)?;
    crate::commands::tag::guard_flat_aggregate_coherence(
        Some(&config),
        workspace.as_ref(),
        &workspace_root,
    )?;

    // An unknown `--crate` is a hard error, shared with release/build/tag:
    // every format's selection silently filters unknown names to an empty
    // set (refresh warns, release-notes renders nothing) and exits 0 — a
    // typo would look like "no changes". The shared-root aggregate's own
    // name (`shared_root_aggregate_name`, the selection rule `tag` also
    // routes through) is valid alongside the universe crates: on
    // lockstep/flat-aggregate shapes it is the one selectable target.
    if let Some(ref name) = crate_name {
        let is_aggregate = crate::commands::tag::shared_root_aggregate_name(
            &workspace_root,
            &config,
            workspace.as_ref(),
        ) == Some(name.as_str());
        if !is_aggregate {
            crate::commands::helpers::validate_selection_against_universe(
                &config,
                std::slice::from_ref(name),
                None,
            )?;
        }
    }

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

/// Resolve the positional `[<tag>|<range>]` into a [`RangeStart`] + upper
/// bound plus an optional crate pin.
///
/// - `None` → `start: Pending`; each target resolves its own last tag (the
///   pending window since its last release).
/// - `..` / `..<ref>` → empty left side ⇒ `start: FullHistory` (no lower
///   bound); `<ref>..` / `<ref>..<ref>` ⇒ `start: Ref(<ref>)`. No pin (applies
///   to all targets).
/// - `<tag>` → resolve the owning crate from its tag prefix, pin to it, and set
///   `start` to `Ref(predecessor)` (the tag immediately below `<tag>` in that
///   crate's semver-sorted list) or `FullHistory` when `<tag>` is the earliest
///   (full history up to it), with `to = <tag>`.
fn resolve_range(
    workspace_root: &Path,
    config: &Config,
    range: Option<&str>,
) -> Result<ResolvedRange> {
    let Some(spec) = range else {
        return Ok(ResolvedRange {
            start: RangeStart::Pending,
            to: None,
            pinned_crate: None,
        });
    };
    if let Some((from, to)) = spec.split_once("..") {
        let start = if from.is_empty() {
            RangeStart::FullHistory
        } else {
            RangeStart::Ref(from.to_string())
        };
        return Ok(ResolvedRange {
            start,
            to: (!to.is_empty()).then(|| to.to_string()),
            pinned_crate: None,
        });
    }
    // Single tag: resolve the owning crate + predecessor. No predecessor means
    // `<tag>` is the earliest tag, so the range is full history up to it.
    let (owning_crate, prefix) = resolve_tag_owner(config, spec)?;
    let predecessor = predecessor_tag(workspace_root, &prefix, spec)?;
    Ok(ResolvedRange {
        start: predecessor.map_or(RangeStart::FullHistory, RangeStart::Ref),
        to: Some(spec.to_string()),
        pinned_crate: Some(owning_crate),
    })
}

/// Map a [`RangeStart`] to the concrete lower-bound ref the engine-backed
/// formats (`keep-a-changelog`, `json`) pass to the changelog engine.
///
/// - `Pending` → the last tag matching `prefix` (`None` when no tags exist yet,
///   which is naturally full history).
/// - `FullHistory` → `None` (no lower bound).
/// - `Ref(r)` → `Some(r)`.
fn resolve_start_bound(
    start: &RangeStart,
    workspace_root: &Path,
    prefix: &str,
) -> Result<Option<String>> {
    match start {
        RangeStart::Pending => find_last_tag_for_prefix(workspace_root, prefix),
        RangeStart::FullHistory => Ok(None),
        RangeStart::Ref(r) => Ok(Some(r.clone())),
    }
}

/// Resolve which crate a single tag belongs to from its tag-template prefix,
/// returning `(crate_name, prefix)`. Mirrors the `resolve-tag` command:
/// longest-matching prefix wins, and the remainder must look like a version.
fn resolve_tag_owner(config: &Config, tag: &str) -> Result<(String, String)> {
    let mut best: Option<(&str, String)> = None;
    for c in config.crate_universe() {
        let prefix = git::per_crate_tag_prefix(&c.name, &c.tag_template);
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

/// The global tag prefix that `tag`/`bump` apply to the lockstep / single
/// unit: the configured `tag.tag_prefix`, defaulting to `v`.
///
/// Shared by [`select_crates`] (lockstep range bounding) and the
/// `run_release_notes` bare-lockstep synthesis so the default-`v` lives in one
/// place; a hardcoded `v` at either site would miss a custom prefix (e.g.
/// `release-v`) and silently degrade the range to full history.
fn global_tag_prefix(config: &Config) -> String {
    config
        .tag
        .as_ref()
        .and_then(|t| t.tag_prefix.clone())
        .unwrap_or_else(|| "v".to_string())
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
        git::per_crate_tag_prefix(&c.name, &c.tag_template)
    };
    let global_prefix = global_tag_prefix(config);
    let entries: Vec<(String, PathBuf, String)> =
        match detect_repo_shape(workspace_root, Some(config), workspace) {
            RepoShape::Single => {
                // The sole crate (or a config-less single crate): one
                // global-prefixed target at its directory (workspace root when no
                // crate is defined). A crate's own `tag_template` still wins when
                // it sets one; otherwise it inherits the global prefix.
                let universe = config.crate_universe();
                match universe.first() {
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
            RepoShape::Lockstep | RepoShape::FlatAggregate(_) => {
                // One aggregate target at the workspace root, using the resolved
                // global `tag.tag_prefix`. Genuine lockstep and a flat-aggregate
                // (shared-prefix flat `crates:` list) both render one flat
                // whole-release section, so both collapse to a single entry here.
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

/// Whether the selected crates render as ONE flat whole-release changelog
/// section (the `single_track` rendering flag).
///
/// [`select_crates`] already collapses every shared-root shape — `Single`,
/// genuine `Lockstep`, and the same-prefix flat-aggregate (`FlatAggregate`) — to
/// exactly ONE workspace-root entry; only a distinct-prefix `PerCrate` yields N.
/// So the flat-aggregate vs multi-track decision is fully made by the shape, and
/// this helper only marks the rendering flag:
/// - `crate_filtered` (an explicit `--crate`/single-tag pin): `false`, so a
///   genuine multi-track repo refreshes only THAT crate's `### <crate>`
///   subsection.
/// - A multi-entry selection (only a distinct-prefix `PerCrate`): `false`,
///   keeping per-crate / multi-track behaviour.
/// - Otherwise a single shared-root entry on shared-root-only routing
///   (`root_enabled && !per_crate`): `true`, forcing the flat roll.
fn resolve_single_track(
    selected: &[(String, PathBuf, String)],
    root_enabled: bool,
    per_crate: bool,
    crate_filtered: bool,
) -> bool {
    if crate_filtered || selected.len() != 1 {
        return false;
    }
    root_enabled && !per_crate
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
    let workspace = load_workspace(workspace_root)?;
    let selected = select_crates(workspace_root, config, workspace.as_ref(), effective_filter);
    if selected.is_empty() {
        log.warn("no crates selected for changelog refresh");
        return Ok(());
    }

    let empty = anodizer_core::config::ChangelogConfig::default();
    let mut routing = ChangelogRouting::from_config(config.changelog.as_ref().unwrap_or(&empty));
    routing.single_track = resolve_single_track(
        &selected,
        routing.root_enabled,
        routing.per_crate,
        effective_filter.is_some(),
    );
    // Derive the root multitrack signal + the full root-routed crate-name set
    // from TOPOLOGY (every configured crate ∩ the root crates filter), so a
    // `--crate`-filtered run still classifies subsections by crate name and a
    // fresh root bootstraps every crate's subsection. One shared full-set source
    // (`config_root_crate_names`) across the refresh / bump / tag callers.
    let root_crate_names =
        crate::commands::changelog_sync::config_root_crate_names(config, routing.root_crates);
    routing.multitrack =
        routing.root_enabled && !routing.single_track && root_crate_names.len() > 1;
    routing.root_crate_names = root_crate_names;

    let targets: Vec<RefreshTarget> = selected
        .into_iter()
        .map(|(name, dir, prefix)| {
            let from_tag = resolve_start_bound(&resolved.start, workspace_root, &prefix)?;
            Ok(RefreshTarget {
                crate_name: name,
                crate_dir: dir,
                from_tag,
                to_ref: resolved.to.clone(),
            })
        })
        .collect::<Result<_>>()?;

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

/// Rewrite `config.crates` into the release-notes render set for an
/// unfiltered run, from the crate universe:
///
/// - empty universe → bare-lockstep / config-less single crate: synthesize
///   ONE project-name aggregate at the workspace root, matching
///   `select_crates`'s lockstep/single arm;
/// - multi-crate universe resolving single-track (a shared-prefix flat
///   aggregate on shared-root-only routing) → collapse to ONE path-cleared
///   crate whose body spans the workspace, instead of N path-filtered
///   duplicates joined by `---` separators. The DECISION lives in
///   `select_crates`/`detect_repo_shape` consumed via
///   `resolve_single_track`, so the predicate can't drift; only the
///   APPLICATION differs because the changelog stage iterates
///   `config.crates` rather than a tuple list;
/// - otherwise → the universe itself, so a crate declared under
///   `workspaces[].crates` gets a release-notes track exactly like a
///   top-level one.
fn materialize_release_notes_render_set(workspace_root: &Path, config: &mut Config) -> Result<()> {
    let universe: Vec<anodizer_core::config::CrateConfig> =
        config.crate_universe().into_iter().cloned().collect();
    if universe.is_empty() {
        let global_prefix = global_tag_prefix(config);
        config.crates = vec![anodizer_core::config::CrateConfig {
            name: config.project_name.clone(),
            path: String::new(),
            tag_template: format!("{}{{{{ Version }}}}", global_prefix),
            ..Default::default()
        }];
        return Ok(());
    }
    let single_track = if universe.len() > 1 {
        let empty = anodizer_core::config::ChangelogConfig::default();
        let routing = ChangelogRouting::from_config(config.changelog.as_ref().unwrap_or(&empty));
        let workspace = load_workspace(workspace_root)?;
        let selected = select_crates(workspace_root, config, workspace.as_ref(), None);
        resolve_single_track(&selected, routing.root_enabled, routing.per_crate, false)
    } else {
        false
    };
    if single_track && let Some(mut first) = universe.first().cloned() {
        first.path = String::new();
        config.crates = vec![first];
    } else {
        config.crates = universe;
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

    // Without an explicit `--crate`, materialize the render set (the
    // changelog stage iterates `config.crates`) so every shape renders the
    // same tracks the kac/json formats derive via
    // `select_crates`/`detect_repo_shape`. Skipped when a `--crate` filter
    // targets a member (handled by the overlay below) so a
    // synthetic/flattened entry never shadows the real per-crate context.
    if effective_filter.is_none() {
        materialize_release_notes_render_set(workspace_root, &mut config)?;
    }

    let selected_crates: Vec<String> = match effective_filter.as_ref() {
        Some(name) => vec![name.clone()],
        None => Vec::new(),
    };

    // Apply the workspace overlay when the filter resolves to a workspace
    // crate so the changelog stage gets the right per-crate context. A
    // top-level entry with the same name wins (the universe's first-seen
    // shadowing), so the overlay engages only when the target is not a
    // top-level crate — covering both pure-`workspaces:` and mixed
    // top-level-plus-`workspaces:` configs.
    if let Some(ref target) = effective_filter
        && let Some(ws) = helpers::workspace_containing_crate(&config, target).cloned()
    {
        log.verbose(&format!(
            "--crate {} lives in workspace '{}'; applying workspace overlay",
            target, ws.name
        ));
        helpers::apply_workspace_overlay(&mut config, &ws);
    }

    // Map the resolved start onto the stage's two signals so release-notes
    // covers the SAME commits the engine-backed formats do for an identical
    // range arg:
    //   - `Ref(r)` → explicit lower bound (`changelog_from`).
    //   - `Pending` → no signal; the stage auto-discovers the last release tag.
    //     `resolve_prev_tag` keeps that tag in snapshot mode (where the current
    //     `Tag` resolves to the latest existing tag and would otherwise be
    //     dropped as "equal to current"), so the pending window stays
    //     "since last release" — matching kac/json's `find_last_tag`.
    //   - `FullHistory` → `changelog_full_history` short-circuits the stage's
    //     auto-discovery so the range spans all history.
    let explicit_from = match &resolved.start {
        RangeStart::Ref(r) => Some(r.clone()),
        RangeStart::Pending | RangeStart::FullHistory => None,
    };
    let ctx_opts = ContextOptions {
        verbose,
        debug,
        selected_crates,
        snapshot,
        // The range start, carried as a dedicated option so the changelog stage
        // overrides its auto-discovered previous tag only when the user supplied
        // an explicit lower bound (the always-auto-populated `PreviousTag`
        // template var cannot signal "user asked for this").
        changelog_from: explicit_from.clone(),
        changelog_full_history: matches!(resolved.start, RangeStart::FullHistory),
        // The explicit upper bound (`<from>..<to>` or a single tag's `to`),
        // threaded into the stage's git-log range so commits AFTER `<to>` are
        // excluded — mirroring how the json format passes `resolved.to` into
        // `render_changelog_json`. `None` (omitted / open `<from>..`) keeps the
        // upper bound at HEAD, preserving the pending window. Setting only the
        // `Tag` template var (below) is insufficient: it drives header/footer
        // text, not the commit-collection range.
        changelog_to: resolved.to.clone(),
        // Standalone-command marker: render the pending window from local git
        // with no release-time preconditions (no checkout, no clean tree, no
        // `changelog.snapshot: true`, no token for github-native). The
        // release/tag pipelines never construct options this way, so their
        // guards stay intact.
        changelog_preview: true,
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
    if let Some(ref f) = explicit_from {
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
    let workspace = load_workspace(workspace_root)?;
    let selected = select_crates(workspace_root, config, workspace.as_ref(), effective_filter);
    // `select_crates` already collapses a flat aggregate to ONE shared-root
    // entry, so the JSON array holds a single whole-release entry rather than N
    // identical per-crate duplicates; only a distinct-prefix `PerCrate` yields N.
    if selected.is_empty() {
        log.warn("no crates selected for changelog json");
    }

    let mut elems: Vec<serde_json::Value> = Vec::new();
    for (name, dir, prefix) in &selected {
        let from_tag = resolve_start_bound(&resolved.start, workspace_root, prefix)?;
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

    #[test]
    fn release_notes_render_set_includes_workspace_crates() {
        // Mixed shape: one top-level crate plus one `workspaces[].crates`
        // member. The render set must carry BOTH so the workspace crate
        // gets its own release-notes track (a top-level-only walk dropped
        // it entirely).
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_name: "proj".to_string(),
            crates: vec![crate_cfg("root", "root-v{{ .Version }}")],
            ..Default::default()
        };
        config.workspaces = Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate_cfg("member", "member-v{{ .Version }}")],
            ..Default::default()
        }]);

        materialize_release_notes_render_set(dir.path(), &mut config).unwrap();

        let names: Vec<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["root", "member"]);
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
    fn resolve_range_none_is_pending() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, None).unwrap();
        assert!(matches!(r.start, RangeStart::Pending));
        assert!(r.to.is_none());
        assert!(r.pinned_crate.is_none());
    }

    #[test]
    fn resolve_range_explicit_range_splits() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, Some("v0.1.0..v0.2.0")).unwrap();
        assert!(matches!(r.start, RangeStart::Ref(ref f) if f == "v0.1.0"));
        assert_eq!(r.to.as_deref(), Some("v0.2.0"));
        assert!(r.pinned_crate.is_none());
    }

    /// An explicit empty left side (`..<ref>`) means full history, distinct
    /// from an omitted arg (which is `Pending` / since-last-release).
    #[test]
    fn resolve_range_open_ended_left_is_full_history() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, Some("..v0.2.0")).unwrap();
        assert!(matches!(r.start, RangeStart::FullHistory));
        assert_eq!(r.to.as_deref(), Some("v0.2.0"));
    }

    /// Bare `..` (empty both sides) is full history to HEAD.
    #[test]
    fn resolve_range_bare_dotdot_is_full_history() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let r = resolve_range(tmp.path(), &cfg, Some("..")).unwrap();
        assert!(matches!(r.start, RangeStart::FullHistory));
        assert!(r.to.is_none());
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

    /// A `WorkspaceInfo` with no `[workspace.package].version`, so the prefix
    /// axis (not the Cargo signal) decides the shape regardless of cwd.
    fn ws_no_lockstep() -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_package_version: None,
            members: vec![],
        }
    }

    /// A same-prefix flat `crates:` list (`FlatAggregate`) collapses through the
    /// REAL production path (`select_crates` → `detect_repo_shape`) to ONE
    /// workspace-root entry keyed by `project_name`, NOT N per-crate entries.
    #[test]
    #[serial]
    fn select_crates_flat_aggregate_collapses_to_one_root_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with_crates(vec![
            crate_cfg("core", "v{{ .Version }}"),
            crate_cfg("cli", "v{{ .Version }}"),
        ]);
        cfg.project_name = "proj".into();
        let selected = select_crates(tmp.path(), &cfg, Some(&ws_no_lockstep()), None);
        assert_eq!(
            selected.len(),
            1,
            "flat aggregate must collapse to one entry"
        );
        assert_eq!(selected[0].0, "proj");
        assert_eq!(selected[0].1, tmp.path());
        assert_eq!(selected[0].2, "v");
    }

    /// A single shared-root entry on shared-root-only routing renders flat.
    #[test]
    fn resolve_single_track_one_shared_root_entry_is_flat() {
        let tmp = tempfile::tempdir().unwrap();
        let selected = vec![("proj".into(), tmp.path().to_path_buf(), "v".into())];
        assert!(resolve_single_track(&selected, true, false, false));
    }

    /// Distinct-prefix multi-track (N entries) is NOT single-track.
    #[test]
    fn resolve_single_track_multi_entry_is_per_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let selected = vec![
            (
                "core".into(),
                tmp.path().join("crates/core"),
                "core-v".into(),
            ),
            ("cli".into(), tmp.path().join("crates/cli"), "cli-v".into()),
        ];
        assert!(!resolve_single_track(&selected, true, false, false));
    }

    /// An explicit `--crate` filter never forces single_track: a genuine
    /// multi-track repo must refresh only that crate's subsection.
    #[test]
    fn resolve_single_track_respects_explicit_crate_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let selected = vec![("core".into(), tmp.path().join("crates/core"), "v".into())];
        assert!(!resolve_single_track(&selected, true, false, true));
    }

    /// Per-crate files configured (`per_crate: true`) keep per-crate behaviour.
    #[test]
    fn resolve_single_track_per_crate_files_is_per_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let selected = vec![("proj".into(), tmp.path().to_path_buf(), "v".into())];
        assert!(!resolve_single_track(&selected, true, true, false));
    }
}
