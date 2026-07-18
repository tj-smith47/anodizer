use super::registry_probe::{
    check_not_burned_on_crates_io, check_not_burned_on_npm_pypi, collect_run_summaries,
    crates_versioned_by_tag,
};
use super::release_probe::check_no_published_releases;
use super::types::{RollbackRefusal, refusal_next_step};
use anodizer_core::log::StageLogger;
use anyhow::Result;

/// Registry probes the published-state guard consults, injected as seams so
/// tests can script registry state without a network (same convention as
/// `gh_binary`).
///
/// Production wiring:
/// - `crates_io` — [`anodizer_stage_publish::cargo::published_on_crates_io`]:
///   `(crate, version) -> published?`. Fail-CLOSED evidence: an `Err` refuses
///   rollback.
/// - `npm` — [`anodizer_stage_publish::npm::version_visible_on_registry`]:
///   `(registry, package, version) -> published?`. Fail-CLOSED like
///   `crates_io`: an npm version is immutable once published, so an `Err`
///   refuses rollback.
/// - `pypi` — [`anodizer_stage_publish::pypi::pypi_version_live`]:
///   `(repository, project, version) -> published?`. Fail-CLOSED like
///   `crates_io`: a PyPI filename is a permanent index slot, so an `Err`
///   refuses rollback.
/// - `chocolatey` —
///   [`anodizer_stage_publish::post_publish::chocolatey::version_blocked_on_gallery`]:
///   `(package id, version) -> Some(blocking state)`. Advisory: an `Err`
///   warns and proceeds (fail open).
/// - `winget` —
///   [`anodizer_stage_publish::post_publish::winget::version_pr_blocking`]:
///   `WingetProbeSpec -> Some(blocking state)`. Advisory, fail open like
///   `chocolatey`.
pub(super) struct BurnProbes<'a> {
    pub(super) crates_io: &'a (dyn Fn(&str, &str) -> Result<bool> + Sync),
    pub(super) npm: &'a (dyn Fn(&str, &str, &str) -> Result<bool> + Sync),
    pub(super) pypi: &'a (dyn Fn(&str, &str, &str) -> Result<bool> + Sync),
    pub(super) chocolatey: &'a (dyn Fn(&str, &str) -> Result<Option<String>> + Sync),
    pub(super) winget: &'a (dyn Fn(&WingetProbeSpec) -> Result<Option<String>> + Sync),
}

/// Coordinates of one winget burn probe, resolved from the crate's
/// `publish.winget` block the same way the publisher resolves its submission
/// target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct WingetProbeSpec {
    /// `<owner>/<repo>` the manifest PR targets
    /// ([`anodizer_stage_publish::winget::resolve_winget_upstream`]).
    pub(super) upstream: String,
    pub(super) package_id: String,
    pub(super) version: String,
    /// Whether the search may keep GitHub's `in:title` qualifier: true only
    /// for the default PR-title format — a custom `commit_msg_template`
    /// makes the title unpredictable, so the search widens to title+body.
    pub(super) search_in_title: bool,
}

/// Ambient GitHub token for the winget burn probe, read through the injected
/// env source. The probe is a read-only public search that works
/// anonymously; the token only buys a higher rate limit.
pub(super) fn winget_probe_token<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> Option<String> {
    anodizer_core::git::resolve_github_token_with_env(None, &|key| env.var(key))
}

/// Concurrency bound for registry burn probes: a large workspace must not
/// open dozens of simultaneous connections against an already-struggling
/// registry.
pub(super) const MAX_PROBE_WORKERS: usize = 8;

/// One pending moderated-registry probe, collected up front so the network
/// round-trips can run on the shared bounded pool.
pub(super) enum ModeratedProbe {
    Chocolatey {
        tag: String,
        id: String,
        version: String,
    },
    Winget {
        tag: String,
        spec: WingetProbeSpec,
    },
}

/// Refuse rollback when any tag's configured chocolatey / winget package is
/// already visible on those registries at the tag's version. Both are true
/// one-way doors (a moderation queue submission or a merged manifest PR
/// blocks re-submitting the same version), and a burn landed by another
/// runner leaves no local summary and no GitHub release — this probe is the
/// only evidence path that can see it.
///
/// Probes run ONLY for crates whose config carries the respective publisher
/// block, and only when the package id is resolvable without a template
/// context (a templated override warns and is skipped). A chocolatey block
/// whose `source_repo` targets a non-community feed is skipped too — private
/// feeds have no community moderation queue, so the gallery page carries no
/// signal for them. The winget probe searches the same upstream repository
/// the publisher would submit to. Unlike the crates.io
/// index probe, an unreachable registry here WARNS AND PROCEEDS (fail open):
/// these endpoints are a moderation-queue HTML page and the rate-limited
/// GitHub search API — flaky enough that failing closed would dead-end
/// legitimate recoveries on transient noise, and both registries' burn is
/// additionally covered by run-summary evidence when the publish ran on this
/// runner. Positive evidence still refuses outright, exactly like a
/// crates.io hit.
pub(super) fn check_not_burned_on_moderated_registries(
    tags: &[String],
    config: &anodizer_core::config::Config,
    probes: &BurnProbes<'_>,
    log: &StageLogger,
) -> Result<()> {
    // Pass 1 — resolve every deduplicated probe up front so the network
    // round-trips can run concurrently on the shared bounded pool.
    let mut pending: Vec<ModeratedProbe> = Vec::new();
    let mut probed: std::collections::HashSet<(&'static str, String, String)> =
        std::collections::HashSet::new();
    for tag in tags {
        for (c, version) in crates_versioned_by_tag(config, tag) {
            if let Some(choco_cfg) = c.publish.as_ref().and_then(|p| p.chocolatey.as_ref()) {
                if !anodizer_stage_publish::chocolatey::targets_community_gallery(choco_cfg) {
                    // Only the community gallery has a moderation queue whose
                    // pending submissions consume a version; a private feed's
                    // state says nothing about the community page.
                    log.verbose(&format!(
                        "skipped the chocolatey gallery burn probe for crate '{}' — its \
                         push target '{}' is not the community gallery",
                        c.name,
                        anodizer_stage_publish::chocolatey::push_source(choco_cfg)
                    ));
                } else {
                    match anodizer_stage_publish::chocolatey::static_package_id(&c.name, choco_cfg)
                    {
                        Some(id) => {
                            if probed.insert(("chocolatey", id.clone(), version.clone())) {
                                pending.push(ModeratedProbe::Chocolatey {
                                    tag: tag.clone(),
                                    id,
                                    version: version.clone(),
                                });
                            }
                        }
                        None => log.warn(&format!(
                            "cannot resolve the chocolatey package id for crate '{}' without a \
                             release template context; skipping its gallery burn probe \
                             (advisory evidence only)",
                            c.name
                        )),
                    }
                }
            }
            if let Some(winget_cfg) = c.publish.as_ref().and_then(|p| p.winget.as_ref()) {
                match anodizer_stage_publish::winget::static_package_identifier(&c.name, winget_cfg)
                {
                    Some(id) => {
                        let (owner, repo) =
                            anodizer_stage_publish::winget::resolve_winget_upstream(winget_cfg);
                        let upstream = format!("{owner}/{repo}");
                        if probed.insert(("winget", format!("{upstream}#{id}"), version.clone())) {
                            pending.push(ModeratedProbe::Winget {
                                tag: tag.clone(),
                                spec: WingetProbeSpec {
                                    upstream,
                                    package_id: id,
                                    version: version.clone(),
                                    search_in_title: winget_cfg.commit_msg_template.is_none(),
                                },
                            });
                        }
                    }
                    None => log.warn(&format!(
                        "cannot resolve the winget package identifier for crate '{}' \
                         without a release template context; skipping its manifest-PR burn \
                         probe (advisory evidence only)",
                        c.name
                    )),
                }
            }
        }
    }
    // Pass 2 — probe the registries concurrently. A worker panic surfaces as
    // an attributed error; per-probe failures stay wrapped so one flaky
    // endpoint cannot abort its siblings.
    let results = anodizer_core::parallel::run_parallel_chunks(
        &pending,
        MAX_PROBE_WORKERS,
        "moderated-registry burn probe",
        log,
        |probe| {
            Ok(match probe {
                ModeratedProbe::Chocolatey { id, version, .. } => (probes.chocolatey)(id, version),
                ModeratedProbe::Winget { spec, .. } => (probes.winget)(spec),
            })
        },
    )?;
    // Pass 3 — classify in the deterministic pass-1 order.
    let mut burned: Vec<String> = Vec::new();
    for (probe, result) in pending.iter().zip(results) {
        match probe {
            ModeratedProbe::Chocolatey { tag, id, version } => match result {
                Ok(Some(state)) => burned.push(format!(
                    "  {tag}: chocolatey package '{id}@{version}' — {state}"
                )),
                Ok(None) => log.status(&format!(
                    "chocolatey has never seen '{id}@{version}' — {tag} \
                     carries no chocolatey one-way door"
                )),
                Err(e) => log.warn(&format!(
                    "could not consult the chocolatey gallery for \
                     '{id}@{version}' ({e:#}); proceeding — this probe is \
                     advisory evidence, and run summaries / crates.io / \
                     GitHub releases still guard the rollback"
                )),
            },
            ModeratedProbe::Winget { tag, spec } => match result {
                Ok(Some(state)) => burned.push(format!(
                    "  {tag}: winget package '{}' at {} — {state}",
                    spec.package_id, spec.version
                )),
                Ok(None) => log.status(&format!(
                    "{} carries no blocking manifest PR for '{} {}' — {tag} \
                     carries no winget one-way door",
                    spec.upstream, spec.package_id, spec.version
                )),
                Err(e) => log.warn(&format!(
                    "could not search {} for '{} {}' ({e:#}); proceeding — this \
                     probe is advisory evidence, and run summaries / crates.io / \
                     GitHub releases still guard the rollback",
                    spec.upstream, spec.package_id, spec.version
                )),
            },
        }
    }
    if !burned.is_empty() {
        return Err(RollbackRefusal {
            reason: format!(
                "these version(s) are already consumed at a moderated one-way-door registry \
                 (submitted by a prior attempt, whatever this run's summaries say):\n{}\n\
                 Those registries never accept the same version twice — a pending \
                 submission blocks a re-push just like an accepted one — so deleting the \
                 tag(s) cannot lead to a clean same-version re-cut — tags kept to protect \
                 the published state.",
                burned.join("\n")
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    Ok(())
}

/// Refuse rollback when the version is already burned at a one-way-door
/// (Submitter group) publisher, by evidence strength:
///
/// 1. Run summaries on disk (`<dist>/run-*/summary.json`, plus
///    `<dist>/<crate>/run-*/summary.json` in per-crate workspaces)
///    whose `tag` matches a tag about to be deleted — the
///    per-publisher truth written by the release run itself, including
///    failed runs. A summary that shows a landed Submitter REFUSES.
/// 2. The immutable one-way-door registries — the crates.io sparse index,
///    the npm registry, and the PyPI index — for every tag that maps (via
///    the repo config's crate tag families) to a crate/entry publishing to
///    them. The run summary answers a PER-RUN question; whether a version is
///    burned on a one-way-door registry is GLOBAL state — a PRIOR run
///    may have published it, and that run's summary lives on another
///    runner. A version live on any of these indexes REFUSES even when
///    this run's summary is clean; an unreachable index FAILS CLOSED
///    (publication state unverifiable). A crates.io tag that maps to NO
///    crate while the config publishes to crates.io also fails closed (the
///    mapping is the probe's eyes), and an npm/pypi package whose name or
///    endpoint is templated (unresolvable outside a release run) fails
///    closed too. A tag whose mapped crates/entries simply don't target
///    an immutable registry carries no such one-way door and proceeds.
/// 3. Only for tags with no matching summary (e.g. a fresh checkout
///    that never ran the release): fall back to probing the GitHub
///    Releases API for a published (non-draft) release at the tag.
///
/// Only a tag that clears every applicable layer is rolled back;
/// reversible-only evidence (github-release assets, blobs,
/// tap/bucket/index commits) permits rollback because their state can
/// be deleted and the same version re-cut.
///
/// Alongside layer 2, the configured moderated registries (chocolatey,
/// winget) are probed as advisory evidence via
/// [`check_not_burned_on_moderated_registries`]: positive evidence refuses
/// like a crates.io hit, but an unreachable registry warns and proceeds.
///
/// `probes` carries the injected registry probes ([`BurnProbes`]) —
/// production wires the stage-publish probe functions; tests inject stubs
/// (same seam convention as `gh_binary`).
///
/// On success returns the subset of `tags` that had NO matching run summary
/// (the "unattributed" tags). The caller uses that to decide release cleanup:
/// a summarized tag's GitHub release belongs to the run being rolled back and
/// may be deleted, while an unattributed tag's release is left untouched.
pub(super) fn check_not_irreversibly_published(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tags: &[String],
    repo_config: &anodizer_core::config::Config,
    probes: &BurnProbes<'_>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let summaries = collect_run_summaries(&resolve_dist_dir(cwd, repo_config), log);
    let mut burned: Vec<(String, Vec<String>)> = Vec::new();
    let mut unsummarized: Vec<String> = Vec::new();
    for tag in tags {
        let matching: Vec<_> = summaries.iter().filter(|s| s.tag == *tag).collect();
        if matching.is_empty() {
            unsummarized.push(tag.clone());
            continue;
        }
        let mut names: Vec<String> = matching
            .iter()
            .flat_map(|s| s.burned_submitter_names())
            .collect();
        names.sort();
        names.dedup();
        // `irreversibly_published` is the precomputed verdict;
        // `burned_submitter_names` additionally catches summaries
        // written before the flag existed.
        if matching.iter().any(|s| s.irreversibly_published) || !names.is_empty() {
            burned.push((tag.clone(), names));
        } else {
            log.status(&format!(
                "no one-way-door publisher landed for {tag} per this run's summary"
            ));
        }
    }
    if !burned.is_empty() {
        let detail = burned
            .iter()
            .map(|(tag, names)| {
                if names.is_empty() {
                    format!("  {tag}: run summary records an irreversible publish")
                } else {
                    format!("  {tag}: version burned at {}", names.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(RollbackRefusal {
            reason: format!(
                "one-way-door publisher(s) already accepted these version(s):\n\
                 {detail}\n\
                 Those registries never accept the same version twice, so deleting the \
                 tag(s) and reverting the bump cannot lead to a clean same-version re-cut \
                 — tags kept to protect the published state."
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    check_not_burned_on_crates_io(tags, &unsummarized, repo_config, probes.crates_io, log)?;
    check_not_burned_on_npm_pypi(tags, repo_config, probes, log)?;
    check_not_burned_on_moderated_registries(tags, repo_config, probes, log)?;
    if unsummarized.is_empty() {
        return Ok(unsummarized);
    }
    let redact_env: Vec<(String, String)> = std::env::vars().collect();
    check_no_published_releases(cwd, gh_binary, &unsummarized, log, &redact_env)?;
    Ok(unsummarized)
}

/// Dist-dir resolution for the published-state guard: the repo config's
/// `dist:`. Relative values anchor at `cwd`.
pub(super) fn resolve_dist_dir(
    cwd: &std::path::Path,
    repo_config: &anodizer_core::config::Config,
) -> std::path::PathBuf {
    let dist = repo_config.dist.clone();
    if dist.is_absolute() {
        dist
    } else {
        cwd.join(dist)
    }
}
