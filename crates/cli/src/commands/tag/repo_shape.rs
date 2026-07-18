use super::*;

/// Repository shape as detected from Cargo.toml + `.anodizer.yaml`.
pub(crate) enum RepoShape {
    /// Single crate or no config — use single-crate path unchanged.
    Single,
    /// `[workspace.package].version` is set — genuine lockstep workspace. One
    /// shared version drives one tag and one changelog, bumped by rewriting the
    /// single root manifest field.
    Lockstep,
    /// A flat `crates:` list of >1 crate whose `tag_template`s ALL yield the
    /// same extractable prefix (one shared `v*` tag namespace) but with per-crate
    /// `[package].version` and no `[workspace.package].version`. Semantically a
    /// lockstep release (one shared tag, one flat changelog), but bumped by
    /// writing N per-crate manifest version fields — so it routes through the
    /// per-crate engine as ONE group rather than the single-manifest lockstep
    /// bump. Carries the flat crate list.
    FlatAggregate(Vec<CrateConfig>),
    /// Each member has its own `[package].version` (no `[workspace.package].version`)
    /// AND the anodizer config has a per-crate/workspace definition.
    /// Carries the groups to iterate: each `Vec<CrateConfig>` is one lockstep
    /// group (singleton = independent release).
    PerCrate(Vec<Vec<CrateConfig>>),
}

/// Detect the repository shape for default (no `--crate`) tag behaviour.
///
/// Reads the Cargo workspace and anodizer config. Precedence:
/// 1. If anodizer config has `workspaces:` with groups → `PerCrate` (hybrid;
///    explicit operator intent, wins over a lockstep `[workspace.package].version`).
///    Top-level `crates:` entries not in any group join as [`prefix_groups`]:
///    each shared-prefix subset is one aggregate group, the rest are
///    singleton groups (independent tracks).
/// 2. If `[workspace.package].version` is set → `Lockstep`.
/// 3. If anodizer config has `crates:` with >1 entry, group by extracted
///    prefix ([`prefix_groups`]):
///    - the WHOLE list sharing ONE explicit tag prefix → `FlatAggregate`
///      (one prefix = one shared tag namespace, so the crates release in
///      lockstep — `v0.2.0` cannot independently belong to two crates — but
///      each carries its own `[package].version`, so it bumps N manifests
///      under one shared tag);
///    - otherwise → `PerCrate` whose tracks are the prefix groups (each
///      shared-prefix subset one aggregate, unique/no-prefix crates
///      singleton).
/// 4. Otherwise → `Single`.
pub(crate) fn detect_repo_shape(
    workspace_root: &Path,
    preloaded_config: Option<&anodizer_core::config::Config>,
    preloaded_workspace: Option<&WorkspaceInfo>,
) -> RepoShape {
    // `.anodizer.yaml`'s `workspaces:` block is an explicit operator
    // declaration of per-crate-with-grouping intent and takes precedence
    // over `[workspace.package].version`, which is often default cruft
    // from `cargo init` that survives even when each crate sets its own
    // `version =`. A repo that genuinely wants lockstep should leave
    // `workspaces:` unset and let the Cargo-level signal speak.
    if let Some(config) = preloaded_config
        && let Some(ref ws_list) = config.workspaces
        && !ws_list.is_empty()
    {
        let mut groups: Vec<Vec<CrateConfig>> =
            ws_list.iter().map(|ws| ws.crates.clone()).collect();
        // Top-level crates alongside `workspaces:` are their own independent
        // tracks — dropping them would leave them untagged forever. One that
        // also appears in a workspace group stays with its group: the group
        // defines its lockstep cadence, and the top-level duplicate is the
        // same crate (the universe's first-seen dedup), not a second track.
        // Leftovers group BY extracted tag prefix (mirroring the flat
        // `crates:` arm below): every subset sharing one prefix lives in one
        // tag namespace and joins as ONE aggregate group — separate singleton
        // groups would cut divergent tags, e.g. v0.2.0 AND v0.1.1, into the
        // same `v*` namespace and cross-contaminate every member's next
        // change detection. A leftover with a unique (or no extractable)
        // prefix stays a singleton group.
        groups.extend(prefix_groups(&leftover_top_level_crates(config)));
        return RepoShape::PerCrate(groups);
    }

    let lockstep = if let Some(ws) = preloaded_workspace {
        ws.workspace_package_version.is_some()
    } else {
        // Unpreloaded fallback (standalone/test callers only — every
        // production caller passes `preloaded_workspace`, and they load it
        // with `?` so a malformed manifest bails before reaching here). A
        // `None` here is an absent Cargo.toml (non-lockstep); even if a
        // parse error were swallowed to `None`, the actual bump execution
        // re-loads via `load_workspace` and bails, so no wrong tag is cut.
        load_workspace(workspace_root)
            .ok()
            .flatten()
            .is_some_and(|ws| ws.workspace_package_version.is_some())
    };
    if lockstep {
        return RepoShape::Lockstep;
    }

    let config = match preloaded_config {
        Some(c) => c,
        None => return RepoShape::Single,
    };

    // Raw `config.crates` walks from here on are the whole universe: a
    // config with `workspaces:` entries returned in the precedence branch
    // above, so no workspace crate can reach this point.
    if config.crates.len() > 1 {
        // A flat `crates:` list groups BY extracted tag prefix: crates
        // sharing one explicit prefix live in one tag namespace — `v0.2.0`
        // cannot simultaneously be two crates' independent tag — so each
        // shared-prefix subset necessarily releases in lockstep as one
        // aggregate group. When the WHOLE list is one such group the shape is
        // the dedicated `FlatAggregate` (one shared tag, one flat changelog);
        // any other split (a shared-prefix subset alongside independent
        // crates, or no sharing at all) is `PerCrate` with the prefix groups
        // as its tracks. Only an EXPLICIT shared prefix aggregates — a crate
        // with no extractable `tag_template` prefix (the per-crate
        // `{crate}-v` fallback) is always its own track.
        let groups = prefix_groups(&config.crates);
        if groups.len() == 1 {
            return RepoShape::FlatAggregate(config.crates.clone());
        }
        return RepoShape::PerCrate(groups);
    }

    RepoShape::Single
}

/// The shared-root aggregate's own selectable name — `project_name` — when
/// the repo shape collapses to ONE workspace-root release unit: `Lockstep`,
/// `FlatAggregate`, or a `Single` with no configured crates. `None` on shapes
/// whose selectable names are the crate universe itself. This is the single
/// selection rule for whether `--crate <project_name>` addresses the
/// aggregate (and therefore the whole repo-level release) rather than a
/// universe crate — the aggregate's name never appears in `crate_universe()`,
/// so validating it there would reject the one name that legitimately selects
/// everything a bare invocation operates on.
pub(crate) fn shared_root_aggregate_name<'a>(
    workspace_root: &Path,
    config: &'a anodizer_core::config::Config,
    workspace: Option<&WorkspaceInfo>,
) -> Option<&'a str> {
    match detect_repo_shape(workspace_root, Some(config), workspace) {
        RepoShape::Lockstep | RepoShape::FlatAggregate(_) => Some(config.project_name.as_str()),
        RepoShape::Single if config.crate_universe().is_empty() => {
            Some(config.project_name.as_str())
        }
        RepoShape::Single | RepoShape::PerCrate(_) => None,
    }
}

/// Group a crate list by extracted tag prefix: every subset of crates whose
/// `tag_template`s yield the same concrete prefix (via
/// [`git::extract_tag_prefix`]) forms ONE group — those crates mint tags into
/// one shared namespace and must release as a lockstep aggregate. A crate
/// with a unique prefix, or with no extractable prefix at all (the per-crate
/// `{crate}-v` fallback keeps such crates in distinct namespaces), stays a
/// singleton group. Group order follows each group's first appearance in
/// `crates`.
///
/// The ONE aggregation decision both [`detect_repo_shape`]'s group building
/// and [`guard_flat_aggregate_coherence`] resolve through — a re-derived
/// predicate at either site could drift and let divergent versions into a
/// shared tag namespace.
pub(crate) fn prefix_groups(crates: &[CrateConfig]) -> Vec<Vec<CrateConfig>> {
    let mut grouped: Vec<(Option<String>, Vec<CrateConfig>)> = Vec::new();
    for c in crates {
        match git::extract_tag_prefix(c.tag_template.as_deref().unwrap_or("")) {
            Some(prefix) => {
                if let Some((_, group)) = grouped
                    .iter_mut()
                    .find(|(p, _)| p.as_deref() == Some(prefix.as_str()))
                {
                    group.push(c.clone());
                } else {
                    grouped.push((Some(prefix), vec![c.clone()]));
                }
            }
            None => grouped.push((None, vec![c.clone()])),
        }
    }
    grouped.into_iter().map(|(_, group)| group).collect()
}

/// The single tag prefix shared by EVERY crate in a flat `crates:` list, or
/// `None` when they do not all share one extractable prefix.
///
/// Returns `Some(prefix)` only when every crate's `tag_template` yields a
/// concrete prefix via [`git::extract_tag_prefix`] AND all those prefixes are
/// equal. A crate whose template has no extractable prefix (so tagging would
/// fall back to a per-crate `{crate}-v`) makes the set non-shared → `None`,
/// since two crates without an explicit common prefix are independent tracks.
pub(crate) fn shared_tag_prefix(crates: &[CrateConfig]) -> Option<String> {
    let mut iter = crates.iter();
    let first = git::extract_tag_prefix(iter.next()?.tag_template.as_deref().unwrap_or(""))?;
    for c in iter {
        if git::extract_tag_prefix(c.tag_template.as_deref().unwrap_or(""))? != first {
            return None;
        }
    }
    Some(first)
}

/// The Cargo-ahead comparison shared by the lockstep and per-crate tagging
/// paths: `candidate` (a bare manifest version) is strictly ahead of the
/// `baseline` `(major, minor, patch)` tuple. Prerelease/build metadata are
/// ignored — the guard compares release lines, matching how a manual
/// `cargo set-version` bump signals the next release. `false` when either
/// side is absent or unparseable (nothing to compare).
pub(crate) fn manifest_version_ahead(
    candidate: Option<&str>,
    baseline: Option<(u64, u64, u64)>,
) -> bool {
    match (candidate.and_then(|v| git::parse_semver(v).ok()), baseline) {
        (Some(c), Some(b)) => (c.major, c.minor, c.patch) > b,
        _ => false,
    }
}

/// The highest readable literal `[package].version` across a per-crate
/// group's members, or `None` when no member has one. A group releases as one
/// lockstep unit (one version for every member), so the guard must compare
/// against the furthest-ahead member — anything lower would downgrade that
/// member's manual bump. Members without a readable literal version (virtual
/// or `version.workspace = true` manifests) contribute nothing, mirroring the
/// coherence guard's skip semantics.
pub(crate) fn group_manifest_version(
    group: &[CrateConfig],
    workspace_root: &Path,
) -> Option<String> {
    group
        .iter()
        .filter_map(|c| {
            let crate_dir = workspace_root.join(&c.path);
            anodizer_stage_build::version_sync::read_cargo_version_opt(&crate_dir.to_string_lossy())
                .ok()
                .flatten()
        })
        .filter_map(|v| git::parse_semver(&v).ok().map(|sv| (v, sv)))
        .max_by_key(|(_, sv)| (sv.major, sv.minor, sv.patch))
        .map(|(raw, _)| raw)
}

/// Top-level `crates:` entries that belong to no `workspaces[].crates` group,
/// deduplicated by name. The one leftover computation both
/// [`detect_repo_shape`]'s group building and
/// [`guard_flat_aggregate_coherence`]'s mixed-shape arm resolve through, so
/// the aggregation decision and its coherence check can never diverge on
/// which crates they consider.
pub(crate) fn leftover_top_level_crates(
    config: &anodizer_core::config::Config,
) -> Vec<CrateConfig> {
    let mut out: Vec<CrateConfig> = Vec::new();
    for c in &config.crates {
        let in_workspace = config
            .workspaces
            .iter()
            .flatten()
            .any(|ws| ws.crates.iter().any(|w| w.name == c.name));
        if !in_workspace && !out.iter().any(|o| o.name == c.name) {
            out.push(c.clone());
        }
    }
    out
}

/// Reject an incoherent shared-prefix aggregate config before any
/// tag/changelog work.
///
/// A crate set whose members share one tag prefix releases under ONE shared
/// tag (e.g. `v0.2.0`). If those members carry DIFFERENT `[package].version`
/// values, that single tag cannot carry two versions — the config is
/// impossible. Read each member's on-disk `[package].version` and bail
/// (listing every member's `crate → version` and the shared prefix, so an
/// N-way divergence is fully visible) when any two disagree; steer the user
/// toward lockstep (`[workspace.package].version`) or independent prefixes.
///
/// Two shapes can be incoherent this way and get the identical check: a
/// `RepoShape::FlatAggregate`, and every multi-member prefix group among a
/// `PerCrate` shape's top-level crates — a shared-prefix SUBSET of a flat
/// `crates:` list, or of the leftovers alongside `workspaces:` (which
/// [`detect_repo_shape`] joins as aggregate groups via the same
/// [`prefix_groups`] decision). Any other shape is a no-op. A member without
/// a readable literal `[package].version` (absent manifest, or a virtual /
/// `version.workspace = true` manifest) is skipped: the guard fires only on
/// genuine version strings it can compare, so a versionless member never
/// trips the check nor masks a real divergence.
pub(crate) fn guard_flat_aggregate_coherence(
    config: Option<&anodizer_core::config::Config>,
    workspace: Option<&WorkspaceInfo>,
    workspace_root: &Path,
) -> Result<()> {
    match detect_repo_shape(workspace_root, config, workspace) {
        RepoShape::FlatAggregate(crates) => {
            check_shared_prefix_version_coherence(&crates, workspace_root)
        }
        RepoShape::PerCrate(_) => {
            let Some(config) = config else {
                return Ok(());
            };
            // Prefix-derived aggregates come from the top-level crate set the
            // shape detection grouped: the leftovers when `workspaces:` is
            // declared, the flat `crates:` list otherwise. Workspace groups
            // themselves are declared lockstep units, not prefix aggregates,
            // and stay outside this guard.
            let candidates = if config.workspaces.as_ref().is_some_and(|w| !w.is_empty()) {
                leftover_top_level_crates(config)
            } else {
                config.crates.clone()
            };
            for group in prefix_groups(&candidates) {
                if group.len() > 1 {
                    check_shared_prefix_version_coherence(&group, workspace_root)?;
                }
            }
            Ok(())
        }
        RepoShape::Single | RepoShape::Lockstep => Ok(()),
    }
}

/// The comparison body shared by [`guard_flat_aggregate_coherence`]'s two
/// aggregate arms: every member with a readable literal `[package].version`
/// must agree, or the shared tag namespace cannot carry the release.
pub(crate) fn check_shared_prefix_version_coherence(
    crates: &[CrateConfig],
    workspace_root: &Path,
) -> Result<()> {
    let prefix = shared_tag_prefix(crates).unwrap_or_else(|| "v".to_string());
    // Read each member's literal `[package].version`, keyed by crate name. Skip
    // members with no readable literal version (no value to compare).
    let mut versions: Vec<(String, String)> = Vec::new();
    for c in crates {
        let crate_dir = workspace_root.join(&c.path);
        if let Ok(Some(ver)) =
            anodizer_stage_build::version_sync::read_cargo_version_opt(&crate_dir.to_string_lossy())
        {
            versions.push((c.name.clone(), ver));
        }
    }
    let Some((_, first_ver)) = versions.first() else {
        return Ok(());
    };
    if versions.iter().any(|(_, v)| v != first_ver) {
        let listing = versions
            .iter()
            .map(|(name, ver)| format!("'{name}' ({ver})"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "crates {listing} share tag prefix '{prefix}' but set different [package].version \
             values; one tag can't carry two versions. For lockstep set \
             [workspace.package].version; for independent releases give each crate a distinct \
             tag_template prefix."
        );
    }
    Ok(())
}
