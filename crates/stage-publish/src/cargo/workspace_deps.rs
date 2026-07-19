//! Dependency-order publishing: resolving intra-workspace dependency pins,
//! waiting for deps to appear on the index, and publish-set completeness checks.

use super::*;

/// Walk `depends_on` from each crate in `seed` to produce a de-duplicated
/// list containing every seed crate plus every transitive dependency that
/// lives in the same config. The `all_crates` slice is searched by name;
/// deps pointing at crates outside the config are ignored (same as cargo's
/// external-dep handling — they're expected to be on crates.io already).
pub(crate) fn expand_with_transitive_deps(
    all_crates: &[CrateConfig],
    seed: &[String],
) -> Vec<String> {
    let name_to_deps: HashMap<&str, &[String]> = all_crates
        .iter()
        .map(|c| (c.name.as_str(), c.depends_on.as_deref().unwrap_or_default()))
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = seed.to_vec();
    while let Some(name) = stack.pop() {
        // Skip names we've already visited or that aren't in the config —
        // external crates.io deps are resolved by cargo against the real
        // registry and don't need to appear in our publish graph.
        if !name_to_deps.contains_key(name.as_str()) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        out.push(name.clone());
        if let Some(deps) = name_to_deps.get(name.as_str()) {
            for dep in *deps {
                if !seen.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// wait_for_workspace_deps — pre-publish polling gate
// ---------------------------------------------------------------------------

/// Parse a crate's `Cargo.toml` for workspace-internal deps that resolve
/// to a literal version pin, filtered to the set of crate names known to
/// the anodize workspace.
///
/// Scans `[dependencies]`, `[dev-dependencies]`, and `[build-dependencies]`
/// (plus their target-specific variants under `[target.*.dependencies]`,
/// etc.). Each `(name, version)` pair captures the package name and version
/// cargo will resolve against the crates.io index at publish time: the name
/// honours `package = "..."` renames (leaf entry, or the workspace-root
/// entry for a `workspace = true` inherit) and the version comes from the
/// literal leaf pin or the workspace root's pin for an inherit. Entries
/// without any resolvable version (git deps, path-only entries, inherits
/// with no root pin) are skipped — there is nothing for the gate to poll
/// for.
///
/// Returns an empty Vec if the manifest can't be read or parsed; the
/// caller logs the case via [`wait_for_workspace_deps`] so the gate
/// degrades to a no-op instead of erroring out a publish that would
/// otherwise have succeeded. `root_cache` shares the parsed workspace-root
/// `[workspace.dependencies]` map across the per-crate calls of one run.
pub(crate) fn workspace_deps_for_crate(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    root_cache: &mut RootDepCache,
) -> Vec<(String, String)> {
    collect_workspace_dep_entries(
        manifest_path,
        workspace_crate_names,
        &["dependencies", "dev-dependencies", "build-dependencies"],
        root_cache,
    )
    .into_iter()
    .filter(|entry| !entry.version.is_empty())
    .map(|entry| (entry.package, entry.version))
    .collect()
}

/// Extract a literal `version = "X.Y.Z"` from a dep value, handling the
/// three shapes cargo accepts:
///
/// - `name = "1.2.3"` — bare string value.
/// - `name = { version = "1.2.3", ... }` — inline table.
/// - `[dependencies.name]\nversion = "1.2.3"` — standard table.
///
/// Returns `None` for `workspace = true` inherits, `git = ...` deps, and
/// path-only entries — none of those produce a crates.io-queryable pin.
pub(crate) fn extract_version_pin(item: &toml_edit::Item) -> Option<String> {
    if let Some(v) = item.as_value() {
        // Bare-string form (`name = "1.2.3"`).
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
        // Inline-table form (`name = { version = "..." }`).
        if let Some(tbl) = v.as_inline_table() {
            // `workspace = true` inherits resolve via the workspace
            // root — no per-dep version pin to poll for here. The
            // sync_workspace_deps path always writes a literal version
            // alongside the inherit when a workspace dep needs pinning,
            // so this branch only fires for inherits with no override.
            if tbl
                .get("workspace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return None;
            }
            return tbl
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    // Standard-table form (`[dependencies.name]` with subkeys).
    if let Some(tbl) = item.as_table() {
        if tbl
            .get("workspace")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        return tbl
            .get("version")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    None
}

/// Pre-publish gate: poll crates.io for every workspace-internal dep at
/// its expected version, blocking until each is queryable. Bails with a
/// loud error after `cfg.resolved_max_wait()` elapses.
///
/// `crate_name` is the crate about to be published (used purely for log
/// context); `deps` is the `(name, version)` set returned by
/// [`workspace_deps_for_crate`] filtered to the anodize workspace.
///
/// No-op when `cfg.resolved_enabled()` is false or `deps` is empty.
pub(crate) fn wait_for_workspace_deps_to_appear(
    crate_name: &str,
    deps: &[(String, String)],
    cfg: &WaitForWorkspaceDepsConfig,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    if !cfg.resolved_enabled() || deps.is_empty() {
        return Ok(());
    }

    let poll_interval = cfg.resolved_poll_interval();
    let max_wait = cfg.resolved_max_wait();
    let deadline = Instant::now() + max_wait;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: wait_for_workspace_deps build HTTP client")?;

    log.status(&format!(
        "gating publish of '{}' on {} workspace dep(s)",
        crate_name,
        deps.len()
    ));

    // Process deps sequentially — the typical fan-in is small (1–3 deps),
    // so per-dep waits compose without needing parallelism. Each dep is
    // polled until found OR the shared deadline elapses, so a slow first
    // dep doesn't extend the total wait beyond `max_wait`.
    for (name, version) in deps {
        let url = sparse_index_url(name);
        log.status(&format!(
            "waiting for {name}@{version} on crates.io (timeout {}s)",
            max_wait.as_secs()
        ));
        loop {
            match probe_dep_on_index(&client, &url, version) {
                Ok(true) => {
                    log.status(&format!(
                        "{name}@{version} available — \
                         continuing publish of '{crate_name}'"
                    ));
                    break;
                }
                Ok(false) => {
                    log.verbose(&format!("{name}@{version} not yet on index — retrying"));
                }
                Err(e) => {
                    log.verbose(&format!(
                        "probe error for {name}@{version}: {e:#} — retrying"
                    ));
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "publish: wait_for_workspace_deps timed out after {}s waiting for \
                     {}@{} (dep of '{}') to appear on crates.io. Either the upstream \
                     publish has not yet landed, or the version pin in {}'s Cargo.toml \
                     does not match what was published. Raise `wait_for_workspace_deps.max_wait` \
                     or verify the upstream Release.yml run completed.",
                    max_wait.as_secs(),
                    name,
                    version,
                    crate_name,
                    crate_name,
                );
            }
            std::thread::sleep(poll_interval);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// publish-set dep-completeness guard
// ---------------------------------------------------------------------------

/// Registry state of a workspace-internal dependency that is NOT in the
/// cargo-publish set, as observed by the guard's index check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DepIndexState {
    /// The dep at the required version is live on crates.io — `cargo publish`
    /// of the dependent will resolve it against the registry. Safe.
    Present,
    /// The dep is positively absent from the index (404, or the version line
    /// is missing). With the dep also absent from the publish set, the real
    /// `cargo publish` would fail with "no matching package". Fail the guard.
    Absent,
    /// The index check could not positively determine presence (transport
    /// error, timeout). Treated conservatively — the guard does NOT fail on
    /// an inconclusive probe, so a transient crates.io outage cannot block a
    /// release whose deps are actually fine.
    Unknown,
}

/// Injectable index presence probe so the guard is unit-testable without a
/// network round-trip. Production wires a closure over [`is_already_published`];
/// tests inject a closure returning canned [`DepIndexState`]s.
pub(crate) type DepIndexProbe<'a> = dyn Fn(&str, &str) -> DepIndexState + 'a;

/// Whether a `[dependencies].<name>` value is a `workspace = true` inherit
/// (dotted `name.workspace = true`, inline `{ workspace = true }`, or a
/// standard sub-table with `workspace = true`).
fn dep_value_is_workspace_inherit(item: &toml_edit::Item) -> bool {
    if let Some(v) = item.as_value()
        && let Some(tbl) = v.as_inline_table()
    {
        return tbl
            .get("workspace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    if let Some(tbl) = item.as_table() {
        return tbl
            .get("workspace")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    false
}

/// A `[workspace.dependencies]` entry as seen from a leaf's
/// `<dep>.workspace = true` inherit: the effective package name (honouring a
/// `package = "..."` rename on the root entry) and its version pin (empty
/// when the entry has no literal pin).
#[derive(Debug, Clone)]
pub(crate) struct RootDepPin {
    package: String,
    version: String,
}

/// Lazily-populated `[workspace.dependencies]` maps, keyed by resolved
/// workspace-root manifest path and shared across the per-crate manifest
/// walks of one publish run: each distinct root is parsed once, and a crate
/// living under a different root (a nested standalone `[workspace]`) can
/// never resolve its inherits against another crate's root. Empty until the
/// first inherit edge forces a parse.
pub(crate) type RootDepCache = HashMap<std::path::PathBuf, HashMap<String, RootDepPin>>;

/// Parse a workspace-root manifest's `[workspace.dependencies]` table into a
/// `key -> RootDepPin` map. The effective name comes from a `package = "..."`
/// rename on the entry (cargo only accepts the rename at the root for
/// inherited deps), falling back to the key. Returns an empty map when the
/// manifest can't be read/parsed or declares no `[workspace.dependencies]`.
fn workspace_dependency_entries(
    workspace_manifest: &std::path::Path,
) -> HashMap<String, RootDepPin> {
    let mut out: HashMap<String, RootDepPin> = HashMap::new();
    let Ok(content) = std::fs::read_to_string(workspace_manifest) else {
        return out;
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return out;
    };
    let Some(ws_deps) = doc
        .get("workspace")
        .and_then(|w| w.as_table_like())
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table_like())
    else {
        return out;
    };
    for (name, value) in ws_deps.iter() {
        let package = value
            .as_table_like()
            .and_then(|t| t.get("package"))
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .to_string();
        let version = extract_version_pin(value).unwrap_or_default();
        out.insert(name.to_string(), RootDepPin { package, version });
    }
    out
}

/// One workspace-internal dependency edge of a crate manifest, as collected
/// by [`collect_workspace_dep_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceDepEntry {
    /// Declaration key in the dependency table — the in-code alias when the
    /// entry carries a `package = "..."` rename, otherwise the crate name.
    key: String,
    /// Effective package name cargo resolves against the registry.
    package: String,
    /// Resolved version pin; empty when no literal pin could be resolved.
    version: String,
}

/// The workspace-internal, publish-required dependencies of one crate: a
/// [`WorkspaceDepEntry`] for every `[dependencies]` / `[build-dependencies]`
/// (incl. their `[target.*]` variants) entry whose effective package name is
/// a workspace crate. `dev-dependencies` are intentionally excluded —
/// `cargo publish` strips them and does NOT require them on the index, so a
/// dev-dep on a sibling that is itself unpublished must not trip the guard.
///
/// The required version is resolved from a literal pin on the leaf entry, or
/// from the workspace root's `[workspace.dependencies]` for a
/// `workspace = true` inherit. An empty version means "the dep edge exists
/// but no registry version could be resolved" — the guard then checks set
/// membership only and skips the (un-versioned) index probe.
fn publish_required_workspace_deps(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    root_cache: &mut RootDepCache,
) -> Vec<WorkspaceDepEntry> {
    collect_workspace_dep_entries(
        manifest_path,
        workspace_crate_names,
        &["dependencies", "build-dependencies"],
        root_cache,
    )
}

/// Walk the given dependency `sections` of one crate manifest (plus their
/// `[target.*.<section>]` variants) and collect a [`WorkspaceDepEntry`] for
/// every entry whose effective package name is a workspace crate.
///
/// The effective name honours `package = "..."` renames: the leaf entry's
/// field for a literal dep, or the workspace-root `[workspace.dependencies]`
/// entry for a `workspace = true` inherit (cargo only accepts the rename at
/// the root for inherited deps), falling back to the declaration key. The
/// version comes from a literal leaf pin, then the root entry's pin for an
/// inherit; entries with no resolvable version are kept with an empty
/// version string so callers can decide between skipping (the wait gate) and
/// membership-only checks (the completeness guard).
///
/// Duplicate package names across sections collapse to one entry; a later
/// occurrence only contributes its version when the first had none. Returns
/// an empty Vec when the manifest can't be read or parsed. `root_cache`
/// shares the parsed `[workspace.dependencies]` maps across the per-crate
/// calls of one run, keyed by each crate's own resolved workspace root.
fn collect_workspace_dep_entries(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    sections: &[&str],
    root_cache: &mut RootDepCache,
) -> Vec<WorkspaceDepEntry> {
    let Ok(content) = std::fs::read_to_string(manifest_path) else {
        return Vec::new();
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };

    // Resolve inherited entries lazily — the root manifest walk happens at
    // most once per crate (memoized below), and the parse at most once per
    // distinct root across the whole run (keyed cache).
    let mut crate_root: Option<Option<std::path::PathBuf>> = None;
    let mut resolve_ws_entry = |dep: &str| -> Option<RootDepPin> {
        let root = crate_root
            .get_or_insert_with(|| {
                find_workspace_root_manifest(
                    manifest_path.parent().unwrap_or(std::path::Path::new(".")),
                )
            })
            .clone()?;
        let map = root_cache
            .entry(root)
            .or_insert_with_key(|m| workspace_dependency_entries(m));
        map.get(dep).cloned()
    };

    let mut out: Vec<WorkspaceDepEntry> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    let mut visit = |item: &toml_edit::Item,
                     out: &mut Vec<WorkspaceDepEntry>,
                     seen: &mut HashMap<String, usize>| {
        let Some(table) = item.as_table_like() else {
            return;
        };
        for (key, value) in table.iter() {
            // A renamed dep uses the TOML key as an alias:
            //   core = { package = "anodizer-core", version = "…" }
            // The crate that must be on the index is `anodizer-core`, not `core`.
            // The rename lives on the leaf entry for a literal dep, or on the
            // workspace-root entry for a `workspace = true` inherit (cargo only
            // accepts `package =` at the root for inherited deps).
            let leaf_package = value
                .as_table_like()
                .and_then(|t| t.get("package"))
                .and_then(|v| v.as_str());
            let root_entry = if leaf_package.is_none() && dep_value_is_workspace_inherit(value) {
                resolve_ws_entry(key)
            } else {
                None
            };
            let package = leaf_package
                .map(str::to_string)
                .or_else(|| root_entry.as_ref().map(|pin| pin.package.clone()))
                .unwrap_or_else(|| key.to_string());
            if !workspace_crate_names.contains(package.as_str()) {
                continue;
            }
            // Literal leaf pin first, then the workspace-root pin for an
            // inherit; an unresolved version stays empty.
            let version = extract_version_pin(value)
                .or_else(|| {
                    root_entry
                        .map(|pin| pin.version)
                        .filter(|ver| !ver.is_empty())
                })
                .unwrap_or_default();
            match seen.get(package.as_str()) {
                Some(&idx) => {
                    // The same package can appear in several sections with
                    // different specs; a version-less first sighting must not
                    // shadow a later pinned one.
                    if out[idx].version.is_empty() && !version.is_empty() {
                        out[idx].version = version;
                    }
                }
                None => {
                    seen.insert(package.clone(), out.len());
                    out.push(WorkspaceDepEntry {
                        key: key.to_string(),
                        package,
                        version,
                    });
                }
            }
        }
    };

    for section in sections {
        if let Some(item) = doc.get(section) {
            visit(item, &mut out, &mut seen);
        }
    }
    // `[target.'cfg(...)'.dependencies]` and friends.
    if let Some(target_item) = doc.get("target")
        && let Some(target_tbl) = target_item.as_table_like()
    {
        for (_cfg, target_value) in target_tbl.iter() {
            let Some(target_table) = target_value.as_table_like() else {
                continue;
            };
            for section in sections {
                if let Some(item) = target_table.get(section) {
                    visit(item, &mut out, &mut seen);
                }
            }
        }
    }
    out
}

/// Pre-publish dep-completeness guard.
///
/// For every crate in the resolved cargo-publish set, walk its
/// `Cargo.toml` non-dev dependencies and assert each workspace-internal
/// dependency is EITHER (a) also in the publish set OR (b) already live on
/// crates.io at the required version. A dep that is in NEITHER would make the
/// real `cargo publish` of the dependent fail with
/// `no matching package named '<dep>' found`, because cargo strips path deps
/// and resolves the version against the crates.io index — exactly the failure
/// that burned the CLI publish on 0.6.0 and 0.7.0 (the stage crates the CLI
/// depends on were missing from the publish set). `cargo publish --dry-run`
/// does NOT catch this: dry-run resolves the dep via the local workspace
/// PATH, so it passes even when the dep is absent from the set and the index.
///
/// `index_probe` is injected so the guard is testable without a network round
/// trip; production wires it over [`is_already_published`]. An inconclusive
/// probe ([`DepIndexState::Unknown`]) never fails the guard — only a positive
/// "absent from BOTH the set AND the index" determination does.
///
/// Works across all config modes: the publish set is whatever
/// [`cargo_publish_plan`] resolved (single-crate, workspace-lockstep, or
/// workspace per-crate), and `all_crates` spans the full universe so the
/// workspace-internal name set is mode-independent.
pub(crate) fn check_publish_set_completeness(
    order: &[String],
    all_crates: &[CrateConfig],
    versions: &HashMap<String, String>,
    index_probe: &DepIndexProbe<'_>,
    log: &StageLogger,
) -> Result<()> {
    // The publish set (names actually being published this run) and the full
    // workspace-internal name set (every crate anodize knows about).
    let in_set: HashSet<&str> = order.iter().map(|s| s.as_str()).collect();
    let workspace_names: HashSet<&str> = all_crates.iter().map(|c| c.name.as_str()).collect();
    let crate_paths: HashMap<&str, &str> = all_crates
        .iter()
        .map(|c| (c.name.as_str(), c.path.as_str()))
        .collect();

    let mut root_cache = RootDepCache::new();
    for publishing in order {
        let path = crate_paths.get(publishing.as_str()).copied().unwrap_or(".");
        let manifest_path = std::path::Path::new(path).join("Cargo.toml");
        let deps =
            publish_required_workspace_deps(&manifest_path, &workspace_names, &mut root_cache);

        for dep in deps {
            let WorkspaceDepEntry {
                key,
                package: dep_name,
                version: required_version,
            } = dep;
            // Surfacing the in-code alias alongside the registry name saves
            // the maintainer a grep when the two differ.
            let alias_note = if key != dep_name {
                format!(" (declared as '{key}' via package rename)")
            } else {
                String::new()
            };
            // In the publish set → the real publish lands it first (topological
            // order guarantees dependency-before-dependent). Safe.
            if in_set.contains(dep_name.as_str()) {
                continue;
            }

            // Not in the set — it must already be on crates.io at the version
            // the dependent requires, or the real publish will 404. Without a
            // resolvable version we cannot probe the exact line; fall back to
            // the dependent's resolved version (lockstep workspaces share one)
            // so the guard still fails loudly on a genuinely-missing sibling
            // rather than silently passing.
            let probe_version = if required_version.is_empty() {
                versions.get(publishing).cloned().unwrap_or_default()
            } else {
                required_version.clone()
            };

            if probe_version.is_empty() {
                // No version to probe AND the dep isn't in the set: we cannot
                // positively prove absence, so do not hard-fail — but surface
                // it so a real gap isn't swallowed silently.
                log.warn(&format!(
                    "crate '{publishing}' depends on workspace crate \
                     '{dep_name}'{alias_note} which is not in the cargo publish set, and the \
                     publish dep-guard could not resolve a required version to verify it is \
                     on crates.io; verify manually"
                ));
                continue;
            }

            match index_probe(&dep_name, &probe_version) {
                DepIndexState::Present => {
                    log.verbose(&format!(
                        "publish dep-guard confirmed '{publishing}' dep '{dep_name}@{probe_version}' is \
                         not in the publish set but is already on crates.io"
                    ));
                }
                DepIndexState::Absent => {
                    anyhow::bail!(
                        "publish dep-guard: crate '{publishing}' depends on workspace crate \
                         '{dep_name}'{alias_note} (version {probe_version}) which is neither in \
                         the cargo \
                         publish set nor already on crates.io; `cargo publish -p {publishing}` \
                         would fail with `no matching package named '{dep_name}' found` because \
                         cargo strips path deps and resolves the version against the crates.io \
                         index.\n\
                         Remediation:\n\
                         1. Add '{dep_name}' to the crates: publish set (give it a publish.cargo \
                         block).\n\
                         2. If '{dep_name}' was intentionally excluded via `skip: true` or an \
                         `if:` condition, verify that the required version was published in a prior \
                         release and is live on crates.io.\n\
                         3. Make the dependency non-publish (feature-gate it or use an external \
                         crate)."
                    );
                }
                DepIndexState::Unknown => {
                    log.warn(&format!(
                        "publish dep-guard could not determine crates.io state for '{publishing}' \
                         dep '{dep_name}@{probe_version}'{alias_note} (transient index error); not \
                         failing the guard on an inconclusive probe — verify the dep is published \
                         if the real `cargo publish` fails"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Injected crate-existence probe for [`check_tp_new_crates`]; production
/// wires it over [`crate_exists_on_index`], tests substitute a closure.
pub(crate) type CrateExistenceProbe<'a> = dyn Fn(&str) -> CrateIndexExistence + 'a;

/// Trusted-Publishing new-crate guard.
///
/// crates.io Trusted Publishing can only publish new VERSIONS of crates that
/// already exist — the TP config attaches to an existing crate, and the
/// first-ever publish of a name requires an API token. A brand-new workspace
/// member in a TP (OIDC) run is therefore guaranteed to 403 partway through
/// the topological publish loop, AFTER its dependencies already landed at the
/// release version. This guard probes the sparse index for every
/// crates.io-targeting crate in the publish set and aborts BEFORE the first
/// publish when any name has never been published, naming the crates and the
/// bootstrap remedy.
///
/// Fail-open by construction: only a definitive index 404
/// ([`CrateIndexExistence::NeverPublished`]) blocks; transport failures
/// ([`CrateIndexExistence::Unknown`]) log at verbose and pass, so an
/// unreachable index can never fail a release whose crates all exist.
/// Crates targeting a non-crates.io registry are skipped entirely — TP is a
/// crates.io mechanism and alternative registries have their own auth.
pub(crate) fn check_tp_new_crates(
    order: &[String],
    cfgs: &HashMap<String, CargoPublishConfig>,
    probe: &CrateExistenceProbe<'_>,
    log: &StageLogger,
) -> Result<()> {
    let mut new_crates: Vec<&str> = Vec::new();
    for name in order {
        if !targets_crates_io(cfgs.get(name)) {
            log.verbose(&format!(
                "publish TP guard: '{name}' targets a non-crates.io registry; skipping \
                 existence probe"
            ));
            continue;
        }
        match probe(name) {
            CrateIndexExistence::Exists => {}
            CrateIndexExistence::NeverPublished => new_crates.push(name),
            CrateIndexExistence::Unknown => {
                log.verbose(&format!(
                    "publish TP guard: could not determine crates.io existence for '{name}' \
                     (transient index error); not failing the guard on an inconclusive probe"
                ));
            }
        }
    }
    if new_crates.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "cargo publish is running under a crates.io Trusted Publishing (OIDC) token, but \
         {crates} {have} never been published: Trusted Publishing cannot CREATE a crate, so \
         {each} would be rejected (403) partway through the publish loop — after {its} \
         dependencies already landed at the release version. Aborting before any crate \
         publishes.\n\
         Remediation, once per new crate:\n\
         1. `cargo publish -p <crate>` with a regular crates.io API token to create the \
         crate (bootstrap publish).\n\
         2. Add the Trusted Publisher config for the new crate on crates.io.\n\
         3. Re-run the release — subsequent versions publish via OIDC normally.",
        crates = new_crates
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(", "),
        have = if new_crates.len() == 1 { "has" } else { "have" },
        each = if new_crates.len() == 1 { "it" } else { "each" },
        its = if new_crates.len() == 1 {
            "its"
        } else {
            "their"
        },
    );
}
