use super::*;
use std::collections::HashMap;

/// The next version `anodizer tag` would cut for one tagging unit.
pub(crate) struct TagPlan {
    /// The tag to create, prefix included (`v1.3.0`, `core-v1.3.0`).
    pub new_tag: String,
    /// The bare version the tag carries (`1.3.0`, `1.3.0-rc`).
    pub new_version: String,
    /// The tag the plan bumps from; empty on a first tag.
    pub old_tag: String,
    /// The version each crate would carry after the writeback, keyed by
    /// crate name. Every crate in a lockstep or single-crate repository;
    /// only the crates of a changed group in a per-crate one.
    pub crate_versions: HashMap<String, String>,
}

/// What the derivation reads once the caller has resolved the previous tag
/// and the bump its commit window asks for.
pub(crate) struct PlanInputs<'a> {
    pub cfg: &'a ResolvedConfig,
    pub prev_tag: Option<&'a str>,
    /// The bump `detect_bump_demoted` read from the commits since `prev_tag`.
    pub bump: BumpKind,
    /// The manifest version of the tagging unit (`[workspace.package].version`
    /// in lockstep mode, the crate's own `[package].version` otherwise).
    pub cargo_current_ver: Option<String>,
    /// A normalized `--version`, which bypasses the bump and the manifest guard.
    pub version_override: Option<String>,
}

/// Derive the next version from the bump signal, the manifest-ahead guard and
/// an explicit override. `None` means nothing asks for a release: no bump
/// signal, the manifest is not ahead of the previous tag, and no override.
pub(crate) fn planned_version(
    inputs: PlanInputs<'_>,
    log: &StageLogger,
) -> Result<Option<TagPlan>> {
    let PlanInputs {
        cfg,
        prev_tag,
        bump,
        cargo_current_ver,
        version_override,
    } = inputs;

    // A manually-bumped manifest strictly ahead of the previous tag is itself
    // a release signal — the operator has set the next version — so autotag
    // never stalls at the old tag after a manual `cargo set-version`.
    let cargo_ahead = manifest_version_ahead(
        cargo_current_ver.as_deref(),
        prev_tag
            .and_then(|t| git::parse_semver_tag(t).ok())
            .map(|p| (p.major, p.minor, p.patch)),
    );

    // An explicit `--version` is itself the release signal, so it tags
    // regardless of any per-commit bump directive.
    if bump == BumpKind::None && !cargo_ahead && version_override.is_none() {
        return Ok(None);
    }

    // With no previous tag, `initial_version` IS the first tag (matching
    // github-tag-action), so it is not bumped.
    let (new_major, new_minor, new_patch, old_tag_str) = if let Some(prev) = prev_tag {
        let base = git::parse_semver_tag(prev)?;
        let (maj, min, pat) = apply_bump(base.major, base.minor, base.patch, &bump);
        (maj, min, pat, prev)
    } else {
        let base = git::parse_semver_tag(&format!("{}{}", cfg.tag_prefix, cfg.initial_version))
            .unwrap_or(git::SemVer {
                major: 0,
                minor: 1,
                patch: 0,
                prerelease: None,
                build_metadata: None,
            });
        (base.major, base.minor, base.patch, "")
    };

    let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);
    if cfg.prerelease {
        new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
    }

    // A manifest version already higher than the tag-derived one wins, so
    // autotag never downgrades a manual bump. Computed even under `--version`
    // so the override warning can name the value it overrides.
    if let Some(cargo_ver) = cargo_current_ver
        && manifest_version_ahead(Some(&cargo_ver), Some((new_major, new_minor, new_patch)))
    {
        if version_override.is_none() {
            log.status(&format!(
                "Cargo.toml version {} > tag-derived {}, using Cargo.toml version",
                cargo_ver, new_version
            ));
        }
        new_version = cargo_ver;
    }

    if let Some(pinned) = version_override {
        if pinned != new_version {
            log.warn(&format!(
                "--version {} overrides the derived version {} (autotag + Cargo.toml-ahead guard bypassed)",
                pinned, new_version
            ));
        }
        new_version = pinned;
    }

    Ok(Some(TagPlan {
        new_tag: format!("{}{}", cfg.tag_prefix, new_version),
        new_version,
        old_tag: old_tag_str.to_string(),
        crate_versions: HashMap::new(),
    }))
}

/// The version a bare `anodizer tag` run from this tree would cut, derived
/// from the config and the tags the push remote still carries (local tags
/// when the remote cannot be listed): no `release_branches` guard and no
/// `--crate` / `--version` narrowing.
///
/// A per-crate or flat-aggregate repository plans one tag per changed group;
/// the plan returned is the highest planned version, and
/// [`TagPlan::crate_versions`] carries each changed crate's own.
/// `None` means no unit carries a release signal.
pub(crate) fn plan_next_version(
    config_override: Option<&Path>,
    log: &StageLogger,
) -> Result<Option<TagPlan>> {
    let opts = TagOpts {
        config_override: config_override.map(Path::to_path_buf),
        ..Default::default()
    };
    let root = crate::commands::helpers::discover_workspace_root(config_override)?;
    let config = load_config_at(&opts, &root)?;
    let workspace = load_workspace(&root)?;
    let mut cfg = ResolvedConfig::from_config(&config, &opts);
    let repo_shape = detect_repo_shape(&root, Some(&config), workspace.as_ref());

    // The remote decides which previous tags count, as it does for
    // `anodizer tag`: a tag deleted there for a re-cut can survive in this
    // clone and would plan the version after the one the remote will cut.
    let remote = opts.push_remote.as_deref().unwrap_or("origin");
    let remote_tag_names: Option<std::collections::HashSet<String>> =
        if git::has_remote_in(&root, remote) {
            match git::list_remote_tag_names_in(&root, remote) {
                Ok(names) => Some(names.into_iter().collect()),
                Err(e) => {
                    log.verbose(&format!(
                        "could not list tags on remote '{remote}' ({e}); the plan reads \
                         local tags, so a tag deleted on the remote may still count"
                    ));
                    None
                }
            }
        } else {
            None
        };

    // A one-entry `crates:` repo tags in that crate's own family (see the
    // same adjustment in `tag::run`).
    if matches!(repo_shape, RepoShape::Single)
        && let [only] = config.crate_universe().as_slice()
    {
        cfg.tag_prefix = git::per_crate_tag_prefix(&only.name, &only.tag_family_template());
    }

    let groups: Option<Vec<Vec<CrateConfig>>> = match repo_shape {
        RepoShape::PerCrate(groups) => Some(groups),
        RepoShape::FlatAggregate(crates) => Some(vec![crates]),
        RepoShape::Single | RepoShape::Lockstep => None,
    };

    if let Some(groups) = groups {
        let results = compute_per_crate_tags(
            &root,
            &groups,
            &opts,
            &cfg,
            config.git.as_ref(),
            Some(&config),
            remote_tag_names.as_ref(),
            log,
        )?;
        let planned: Vec<(git::SemVer, &str, &GroupTagResult)> = results
            .iter()
            .filter_map(|group| {
                let (tag, _) = group.new_tags.first()?;
                let semver = git::parse_semver_tag(tag).ok()?;
                Some((semver, tag.as_str(), group))
            })
            .collect();
        let crate_versions: HashMap<String, String> = planned
            .iter()
            .flat_map(|(semver, _, group)| {
                group
                    .crate_names
                    .iter()
                    .map(move |name| (name.clone(), semver.version_string()))
            })
            .collect();
        let newest = planned.iter().max_by(|a, b| a.0.cmp(&b.0));
        return Ok(newest.map(|(semver, tag, group)| TagPlan {
            new_tag: tag.to_string(),
            new_version: semver.version_string(),
            old_tag: group.prev_tag.clone().unwrap_or_default(),
            crate_versions,
        }));
    }

    let prev_tag = find_previous_tag(&cfg, config.git.as_ref(), remote_tag_names.as_ref())?;
    if let Some(ref tag) = prev_tag
        && !git::has_commits_since_tag_in(&root, tag)?
    {
        let force = if cfg.prerelease {
            cfg.force_without_changes_pre
        } else {
            cfg.force_without_changes
        };
        if !force {
            return Ok(None);
        }
    }
    let messages = get_messages_for_bump(&root, &cfg, prev_tag.as_deref(), None)?;
    let plan = planned_version(
        PlanInputs {
            cfg: &cfg,
            prev_tag: prev_tag.as_deref(),
            bump: detect_bump_demoted(&messages, &cfg, prev_tag.as_deref()),
            cargo_current_ver: workspace
                .as_ref()
                .and_then(|ws| ws.workspace_package_version.clone()),
            version_override: None,
        },
        log,
    )?;
    // One version for the whole repository: every crate is written to it.
    Ok(plan.map(|mut plan| {
        plan.crate_versions = config
            .crate_universe()
            .iter()
            .map(|c| (c.name.clone(), plan.new_version.clone()))
            .collect();
        plan
    }))
}
