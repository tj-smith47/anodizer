use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Optional extended fields for manifest generation.
#[derive(Default)]
pub(crate) struct ManifestOptions<'a> {
    /// Explicit homepage URL.  Falls back to the GitHub release URL when available.
    pub(crate) homepage: Option<&'a str>,
    /// GitHub owner/name for default homepage fallback (e.g. "owner/repo").
    pub(crate) github_slug: Option<String>,
    /// Data paths persisted between updates.
    pub(crate) persist: Option<&'a [String]>,
    /// Application dependencies.
    pub(crate) depends: Option<&'a [String]>,
    /// Commands to run before installation.
    pub(crate) pre_install: Option<&'a [String]>,
    /// Commands to run after installation.
    pub(crate) post_install: Option<&'a [String]>,
    /// Start menu shortcuts.
    pub(crate) shortcuts: Option<&'a [Vec<String>]>,
    /// Binary names (without `.exe` extension) to use in the `bin` field.
    /// When set, these are used instead of deriving from the manifest name.
    /// Multiple entries produce a JSON array in the `bin` field.
    pub(crate) bin: Option<&'a [String]>,
}

/// A single architecture entry for the Scoop manifest.
pub(crate) struct ArchEntry {
    /// Scoop architecture key: "64bit", "32bit", or "arm64".
    pub(crate) scoop_arch: String,
    pub(crate) url: String,
    pub(crate) hash: String,
    /// When the archive wraps contents in a top-level directory, this holds that
    /// directory name.  Bin entries will be prefixed with it (e.g. `dir/bin.exe`).
    pub(crate) wrap_in_directory: Option<String>,
}

/// Generate a single-architecture Scoop JSON manifest string for a Windows
/// binary. A thin wrapper over [`generate_manifest_with_opts`] that the unit
/// tests use to exercise manifest shape without assembling an `ArchEntry` set;
/// the production publish path always renders through
/// [`generate_manifest_with_opts`] directly.
#[cfg(test)]
pub(crate) fn generate_manifest(
    name: &str,
    version: &str,
    url: &str,
    hash: &str,
    description: &str,
    license: &str,
) -> Result<String> {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: url.to_string(),
        hash: hash.to_string(),
        wrap_in_directory: None,
    }];
    generate_manifest_with_opts(
        name,
        version,
        &entries,
        description,
        license,
        &ManifestOptions::default(),
    )
}

/// Generate a Scoop JSON manifest string with extended options.
///
/// Accepts multiple architecture entries. Each entry maps to a key in
/// the `architecture` block: `64bit`, `32bit`, or `arm64`.
pub(crate) fn generate_manifest_with_opts(
    name: &str,
    version: &str,
    arch_entries: &[ArchEntry],
    description: &str,
    license: &str,
    opts: &ManifestOptions<'_>,
) -> Result<String> {
    // Homepage: explicit > GitHub owner/repo > bare name fallback.
    let default_homepage = opts
        .github_slug
        .as_deref()
        .map(|slug| format!("https://github.com/{}", slug))
        .unwrap_or_else(|| format!("https://github.com/{}", name));
    let homepage = opts.homepage.unwrap_or(&default_homepage);

    // Scoop bin entry: use explicit binary names when provided, otherwise
    // derive from the manifest name. Append `.exe` only if not already present.
    let ensure_exe = |b: &str| -> String {
        if b.ends_with(".exe") {
            b.to_string()
        } else {
            format!("{}.exe", b)
        }
    };

    // Compute bin value for a given wrap_in_directory prefix.
    // When wrap_in_directory is set, each bin entry becomes a pair:
    //   ["wrap_dir/binary.exe", "alias"]
    // where alias is the binary name without the .exe extension.
    let make_bin_value = |wrap_dir: Option<&str>| -> serde_json::Value {
        let raw_bins: Vec<String> = match opts.bin {
            Some(bins) if !bins.is_empty() => bins.iter().map(|b| ensure_exe(b)).collect(),
            _ => vec![ensure_exe(name)],
        };
        match wrap_dir {
            Some(dir) if !dir.is_empty() => {
                let pairs: Vec<serde_json::Value> = raw_bins
                    .iter()
                    .map(|exe| {
                        let alias = exe.strip_suffix(".exe").unwrap_or(exe);
                        // filepath.ToSlash → forward-slash.
                        serde_json::json!([format!("{}/{}", dir, exe), alias])
                    })
                    .collect();
                serde_json::json!(pairs)
            }
            _ => {
                // `bin` is always emitted as an array, even for a single
                // binary. Manifest validators that pin the schema to
                // `array of strings` reject the singleton-string form.
                serde_json::json!(raw_bins)
            }
        }
    };

    // Build the architecture block from entries.
    let mut arch_obj = serde_json::Map::new();
    for entry in arch_entries {
        let bin_value = make_bin_value(entry.wrap_in_directory.as_deref());
        arch_obj.insert(
            entry.scoop_arch.clone(),
            serde_json::json!({
                "url": entry.url,
                "hash": entry.hash,
                "bin": bin_value
            }),
        );
    }

    let mut manifest = serde_json::json!({
        "version": version,
        "description": description,
        "homepage": homepage,
        "license": license,
        "architecture": arch_obj
    });

    // Add optional array fields when present. The manifest above is constructed
    // from a `serde_json::json!({...})` object literal; `as_object_mut()` cannot
    // return None unless that literal is changed to a non-object form.
    let obj = manifest
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("scoop: manifest root is not a JSON object"))?;

    if let Some(persist) = opts.persist {
        obj.insert("persist".to_string(), serde_json::json!(persist));
    }
    if let Some(depends) = opts.depends {
        obj.insert("depends".to_string(), serde_json::json!(depends));
    }
    if let Some(pre_install) = opts.pre_install {
        obj.insert("pre_install".to_string(), serde_json::json!(pre_install));
    }
    if let Some(post_install) = opts.post_install {
        obj.insert("post_install".to_string(), serde_json::json!(post_install));
    }
    if let Some(shortcuts) = opts.shortcuts {
        obj.insert("shortcuts".to_string(), serde_json::json!(shortcuts));
    }

    serde_json::to_string_pretty(&manifest).context("scoop: serialize manifest")
}

// ---------------------------------------------------------------------------
// Multi-artifact disambiguation
// ---------------------------------------------------------------------------

/// Format preference for scoop buckets: `.zip` (canonical on Windows) first,
/// then `.tar.gz` / `tgz` as a fallback.
pub(crate) const SCOOP_PREFERRED_FORMATS: &[&str] = &["zip", "tar.gz", "tgz"];

/// Disambiguate a list of `(ArchEntry, format)` pairs when the same
/// `scoop_arch` key appears more than once. Delegates to
/// [`crate::util::disambiguate_by_format`].
pub(crate) fn disambiguate_arch_entries(
    entries: Vec<(ArchEntry, String)>,
    ids_was_set: bool,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<ArchEntry>> {
    let deduped = crate::util::disambiguate_by_format(
        entries,
        |(entry, _)| entry.scoop_arch.clone(),
        |(_, fmt)| fmt.as_str(),
        |(entry, _)| entry.url.clone(),
        crate::util::DisambiguateConfig {
            preferred_formats: SCOOP_PREFERRED_FORMATS,
            ids_was_set,
            publisher_label: "scoop",
            crate_name,
            logger: log,
        },
    )?;
    Ok(deduped.into_iter().map(|(entry, _fmt)| entry).collect())
}

// ---------------------------------------------------------------------------
// Windows-artifact eligibility (shared by the live collector + schema guard)
// ---------------------------------------------------------------------------

/// True when an artifact is a Windows build — by target triple or by path —
/// i.e. one the scoop bucket manifest's `architecture` block consumes.
///
/// The single home for this classification so the live `publish_to_scoop`
/// collector and the offline schema validator's snapshot-shard guard agree on
/// which artifacts feed a scoop manifest; if Windows detection later changes,
/// both update together rather than the guard silently suppressing validation
/// of an artifact that would publish.
fn is_scoop_windows_artifact(a: &anodizer_core::artifact::Artifact) -> bool {
    a.target
        .as_deref()
        .map(|t| t.to_ascii_lowercase().contains("windows"))
        .unwrap_or(false)
        || a.path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("windows")
}

/// Artifact-selection filters for scoop: Windows-only, the
/// `only_replacing_unibins` universal-binary rule, an optional `ids` allow-list,
/// and `amd64_variant` microarchitecture selection.
struct ScoopArtifactFilters<'a> {
    ids: Option<&'a [String]>,
    amd64_variant: Option<&'a str>,
}

impl<'a> ScoopArtifactFilters<'a> {
    fn matches(&self, a: &anodizer_core::artifact::Artifact) -> bool {
        // OnlyReplacingUnibins: exclude universal binaries that didn't replace
        // single-arch variants.
        if !a.only_replacing_unibins() {
            return false;
        }
        if !is_scoop_windows_artifact(a) {
            return false;
        }
        if let Some(ids) = self.ids {
            let matched = a
                .metadata
                .get("id")
                .map(|id| ids.iter().any(|i| i == id))
                .unwrap_or(false);
            if !matched {
                return false;
            }
        }
        let target = a.target.as_deref().unwrap_or("");
        let (_, arch) = anodizer_core::target::map_target(target);
        if arch == "amd64"
            && let Some(want) = self.amd64_variant
            && a.metadata.get("amd64_variant").is_some_and(|v| v != want)
        {
            return false;
        }
        true
    }

    /// Derive the scoop artifact filters from a crate's scoop config, applying
    /// the `amd64_variant` default (`v1`) once so the live collector and the
    /// schema validator's shard-guard cannot disagree on which artifacts are
    /// eligible.
    fn from_config(scoop_cfg: &'a anodizer_core::config::ScoopConfig) -> Self {
        ScoopArtifactFilters {
            ids: scoop_cfg.ids.as_deref(),
            amd64_variant: scoop_cfg.amd64_variant.as_deref().or(Some("v1")),
        }
    }
}

/// True when `crate_name` has at least one Windows archive artifact this run
/// would feed into a scoop manifest, after the same `ids` / `amd64_variant`
/// filters [`publish_to_scoop`] applies.
///
/// A real release always produces one (the publish path errors otherwise), but
/// a single-target / sharded snapshot legitimately builds only one platform —
/// so the offline schema validator consults this to skip a crate whose Windows
/// archive was not built in the current shard rather than fail on the
/// publisher's own "no Windows archive artifact" guard.
pub(crate) fn crate_has_scoop_artifacts(
    ctx: &Context,
    crate_name: &str,
    scoop_cfg: &anodizer_core::config::ScoopConfig,
) -> bool {
    let filters = ScoopArtifactFilters::from_config(scoop_cfg);
    let artifact_kind = util::resolve_artifact_kind(scoop_cfg.use_artifact.as_deref());
    ctx.artifacts
        .by_kind_and_crate(artifact_kind, crate_name)
        .iter()
        .any(|a| filters.matches(a))
}

// ---------------------------------------------------------------------------
// render_scoop_manifest_for_crate
// ---------------------------------------------------------------------------

/// Resolve a crate's scoop config and render its bucket manifest in-memory,
/// with no clone, disk, or network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip_upload`
/// or a falsy `if` condition). Errors when the crate carries no `scoop` block,
/// or when a matched Windows archive is missing its `sha256` metadata (which
/// would render a manifest `scoop install` rejects). The live publish path and
/// the offline schema validator both call this so the validated document is
/// byte-for-byte what a real publish would push.
pub(crate) fn render_scoop_manifest_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "scoop")?;
    let scoop_cfg = publish
        .scoop
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no scoop config for '{}'", crate_name))?;

    // Check skip_upload / `if:` gate before doing any work.
    let label = format!("scoop publisher for crate '{}'", crate_name);
    if util::should_skip_publisher_with_if(
        ctx,
        None,
        scoop_cfg.skip_upload.as_ref(),
        scoop_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let version = ctx.version();

    // Fall back to project `metadata.*` when scoop config unset.
    let description_raw = scoop_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description = util::render_or_warn(ctx, log, "scoop.description", description_raw)?;

    // Scoop manifest schema lists `license` under `["string", "object"]` but
    // does NOT mark it required (see ScoopInstaller/Scoop schema.json — only
    // `version`, `homepage`, `bin`/`shortcuts` are required). Empty string is
    // a tolerated default; the bucket renders "no license" in the gallery UI.
    let license = scoop_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string))
        .unwrap_or_default();

    // Use name override if set, otherwise crate name; render through template engine.
    let manifest_name_raw = scoop_cfg.name.as_deref().unwrap_or(crate_name);
    let manifest_name_rendered = util::render_or_warn(ctx, log, "scoop.name", manifest_name_raw)?;
    let manifest_name = manifest_name_rendered.as_str();

    // Find all Windows Archive artifacts, applying IDs + amd64_variant filter.
    let url_template = scoop_cfg.url_template.as_deref();
    let filters = ScoopArtifactFilters::from_config(scoop_cfg);

    let artifact_kind = util::resolve_artifact_kind(scoop_cfg.use_artifact.as_deref());
    let all_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);

    let raw_arch_entries: Vec<(ArchEntry, String)> = all_artifacts
        .into_iter()
        .filter(|a| filters.matches(a))
        .map(|a| -> Result<(ArchEntry, String)> {
            let target = a.target.as_deref().unwrap_or("");
            let (_, raw_arch) = anodizer_core::target::map_target(target);

            // Map architecture to Scoop keys.
            let scoop_arch = match raw_arch.as_str() {
                "amd64" => "64bit",
                "386" => "32bit",
                "arm64" => "arm64",
                _ => "64bit",
            };

            // Resolve download URL: use url_template if set, otherwise artifact metadata.
            let url = if let Some(tmpl) = url_template {
                util::render_url_template_with_ctx(
                    ctx,
                    tmpl,
                    manifest_name,
                    &version,
                    &raw_arch,
                    "windows",
                )
            } else {
                a.metadata
                    .get("url")
                    .cloned()
                    .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
            };

            let hash = a
                .metadata
                .get("sha256")
                .cloned()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "scoop: artifact '{}' for crate '{}' is missing required sha256 \
                         metadata. The generated bucket manifest would publish with \
                         architecture.hash: '' and `scoop install` rejects manifests \
                         whose hash field is empty (verify step fails before download \
                         proceeds). This indicates the artifacts.json catalog dropped \
                         the entry's sha256 before the publish stage. Re-run with \
                         `task release` from a clean dist/ and verify dist/artifacts.json \
                         carries metadata.sha256 for every Windows artifact.",
                        a.name(),
                        crate_name,
                    )
                })?;
            let wrap_in_directory = a.metadata.get("wrap_in_directory").cloned();
            // `format` is consumed by the multi-archive disambiguator (preferred:
            // .zip > .tar.gz > .tgz). Empty value just demotes this entry to the
            // lowest preference tier — it does not ship anywhere downstream.
            let format = a.metadata.get("format").cloned().unwrap_or_default();

            Ok((
                ArchEntry {
                    scoop_arch: scoop_arch.to_string(),
                    url,
                    hash,
                    wrap_in_directory,
                },
                format,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    if raw_arch_entries.is_empty() {
        anyhow::bail!(
            "scoop: no Windows archive artifact found for crate '{}'",
            crate_name
        );
    }

    // Disambiguate: when ids: is unset and multiple archives share a scoop_arch
    // key, prefer .zip then .tar.gz over other formats.
    let arch_entries = disambiguate_arch_entries(
        raw_arch_entries,
        scoop_cfg.ids.as_deref().is_some(),
        crate_name,
        log,
    )?;

    // Collect binary names from artifact metadata. The archive stage stores
    // the binary name in the `"binary"` metadata key. Deduplicate to a unique
    // set of binary names across all architecture variants.
    //
    // Gated on the same `filters.matches` the arch-entry collector above
    // applies — not a looser Windows-only check — so a binary name from an
    // artifact that `ids` / `amd64_variant` excluded cannot leak into the
    // manifest's `bin` field while that artifact's arch entry is (correctly)
    // absent.
    let bin_names: Vec<String> = {
        let mut names = Vec::new();
        let all_win = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);
        for a in &all_win {
            if !filters.matches(a) {
                continue;
            }
            if let Some(bin) = a.metadata.get("binary")
                && !names.contains(bin)
            {
                names.push(bin.clone());
            }
        }
        names
    };
    let bin_names_ref: Option<&[String]> = if bin_names.is_empty() {
        None
    } else {
        Some(&bin_names)
    };

    // Derive GitHub slug (owner/repo) for homepage fallback.
    let github_slug = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));

    // Template-render homepage so users can write
    // `homepage: "https://{{ .Env.HOSTED_DOMAIN }}/{{ .ProjectName }}"`.
    // Name, Description, Homepage, and SkipUpload are all template-rendered.
    let homepage_raw = scoop_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(crate_name));
    let homepage_rendered = match homepage_raw {
        Some(h) => Some(
            ctx.render_template(h)
                .with_context(|| format!("scoop: render homepage template for '{crate_name}'"))?,
        ),
        None => None,
    };
    let opts = ManifestOptions {
        homepage: homepage_rendered.as_deref(),
        github_slug,
        persist: scoop_cfg.persist.as_deref(),
        depends: scoop_cfg.depends.as_deref(),
        pre_install: scoop_cfg.pre_install.as_deref(),
        post_install: scoop_cfg.post_install.as_deref(),
        shortcuts: scoop_cfg.shortcuts.as_deref(),
        bin: bin_names_ref,
    };

    let manifest = generate_manifest_with_opts(
        manifest_name,
        &version,
        &arch_entries,
        &description,
        &license,
        &opts,
    )?;

    Ok(Some(manifest))
}

// ---------------------------------------------------------------------------
// publish_to_scoop
// ---------------------------------------------------------------------------

/// Render and push the Scoop manifest for `crate_name`.
///
/// Returns `Ok(true)` when an actual git push was made to the bucket
/// repo; `Ok(false)` when the publish was skipped (skip_upload, dry-run,
/// or any future early-exit guard). The caller (Publisher::run) uses
/// the boolean to decide whether to record rollback evidence — see
/// `publish_to_homebrew` for the long-form rationale.
pub fn publish_to_scoop(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "scoop")?;

    let scoop_cfg = publish
        .scoop
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no scoop config for '{}'", crate_name))?;

    // Check skip_upload / `if:` gate before doing any work, matching the order
    // the shared renderer applies — so a skipped crate short-circuits before
    // repo resolution or the dry-run log line, exactly as before.
    let label = format!("scoop publisher for crate '{}'", crate_name);
    if util::should_skip_publisher_with_if(
        ctx,
        None,
        scoop_cfg.skip_upload.as_ref(),
        scoop_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(false);
    }

    let (repo_owner, repo_name) =
        crate::util::resolve_repo_owner_name(scoop_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("scoop: no repository config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Scoop bucket {}/{} for '{}'",
            repo_owner, repo_name, crate_name
        ));
        return Ok(false);
    }

    let version = ctx.version();

    // Use name override if set, otherwise crate name; render through template
    // engine. Recomputed here (cheap) because the manifest filename and commit
    // message key off the rendered name; the manifest body itself is rendered
    // by `render_scoop_manifest_for_crate`.
    let manifest_name_raw = scoop_cfg.name.as_deref().unwrap_or(crate_name);
    let manifest_name_rendered = util::render_or_warn(ctx, log, "scoop.name", manifest_name_raw)?;
    let manifest_name = manifest_name_rendered.as_str();

    // Render the manifest via the same path the schema validator uses. The
    // skip_upload / `if:` gate was already evaluated above; the renderer
    // re-checks it (returning None) but on this path it always yields Some.
    let Some(manifest) = render_scoop_manifest_for_crate(ctx, crate_name, log)? else {
        return Ok(false);
    };

    // Clone bucket repo, write manifest, commit, push.
    let token = util::resolve_repo_token(
        ctx,
        scoop_cfg.repository.as_ref(),
        Some("SCOOP_BUCKET_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        ctx,
        scoop_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "scoop",
        log,
    )?;

    // Place manifest in optional subdirectory.
    let manifest_dir = if let Some(dir) = scoop_cfg.directory.as_deref() {
        let d = repo_path.join(dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("scoop: create directory {}", d.display()))?;
        d
    } else {
        repo_path.to_path_buf()
    };

    let manifest_path = manifest_dir.join(format!("{}.json", manifest_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    log.status(&format!("wrote Scoop manifest {}", manifest_path.display()));

    let scoop_default = "Scoop update for {{ ProjectName }} version {{ Tag }}";
    let commit_msg = crate::homebrew::render_commit_msg(
        Some(
            scoop_cfg
                .commit_msg_template
                .as_deref()
                .unwrap_or(scoop_default),
        ),
        manifest_name,
        &version,
        "manifest",
        log,
        ctx.render_is_strict(),
    )?;

    let manifest_lossy = manifest_path.to_string_lossy();
    let commit_opts = util::resolve_commit_opts(ctx, scoop_cfg.commit_author.as_ref(), log)?;
    let branch = util::resolve_branch(ctx, scoop_cfg.repository.as_ref());
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &[&manifest_lossy],
        &commit_msg,
        branch.as_deref(),
        "scoop",
        &commit_opts,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Scoop bucket {}/{} updated for '{}'",
                repo_owner, repo_name, crate_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, scoop manifest for '{}' already up to date",
                manifest_name
            ));
        }
    }

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.as_deref().unwrap_or("main");
    // Clone the repository config so the `maybe_submit_pr` call no
    // longer borrows from `ctx.config` (via `scoop_cfg`). NLL then
    // drops the immutable borrow, making the subsequent `&mut ctx`
    // call legal.
    let repo_for_pr = scoop_cfg.repository.clone();
    let pr_outcome = util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
            branch_name: pr_branch,
            // Scoop publishes commit directly to the bucket branch;
            // the optional PR is informational. The winget/krew/cask
            // `update_existing_pr:` flag has no analogue on
            // `ScoopConfig` because there's no real "blocked queue" to
            // recover from here.
            update_existing_pr: false,
        },
        &format!("Update {} manifest to {}", manifest_name, version),
        &format!(
            "## Manifest\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
            manifest_name, version
        ),
        "scoop",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }

    Ok(outcome.is_pushed())
}

// ---------------------------------------------------------------------------
// ScoopPublisher — Publisher trait wrapper (git-revert rollback)
// ---------------------------------------------------------------------------

/// Scoop bucket publisher. Mirrors the `homebrew` shape: each pushed
/// manifest is recorded so a `--rollback-only` re-clones the bucket,
/// runs `git revert HEAD --no-edit`, and pushes the revert.
///
/// Scoop is always per-crate (no top-level Scoop config block), so
/// the run loop only walks `ctx.config.crates`.
///
/// CREDENTIAL HANDLING: [`ScoopTarget`] stores `token_env_var` — the
/// NAME of the env var — not the resolved token VALUE. The token is
/// read from the live env at rollback time so persisted evidence
/// carries no secret material. Same rule applies to the homebrew /
/// nix git-revert publishers.
use crate::util::{RevertTarget, run_revert_targets_parallel};

simple_publisher!(
    ScoopPublisher,
    "scoop",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. See the homebrew publisher for the same
/// pattern.
type ScoopTarget = anodizer_core::publish_evidence::ScoopTargetSnapshot;

fn decode_scoop_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<ScoopTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Scoop(s) => s.scoop_targets.clone(),
        _ => Vec::new(),
    }
}

/// Collapse recorded bucket-push targets to a unique set keyed by
/// `(repo_url, branch)`. First entry seen wins. See homebrew's
/// `dedup_homebrew_targets` for the same-revert-twice hazard.
fn dedup_scoop_targets(targets: &[ScoopTarget]) -> Vec<ScoopTarget> {
    let mut seen: std::collections::BTreeSet<(String, Option<String>)> =
        std::collections::BTreeSet::new();
    let mut out: Vec<ScoopTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        let key = (t.repo_url.clone(), t.branch.clone());
        if seen.insert(key) {
            out.push(t.clone());
        }
    }
    out
}

fn collect_scoop_run_targets(ctx: &Context) -> Vec<ScoopTarget> {
    let mut out: Vec<ScoopTarget> = Vec::new();
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(sc) = c.publish.as_ref().and_then(|p| p.scoop.as_ref()) else {
            continue;
        };
        if let Some((owner, name)) = util::resolve_repo_owner_name(sc.repository.as_ref()) {
            out.push(ScoopTarget {
                target: c.name.clone(),
                repo_url: format!("https://github.com/{}/{}.git", owner, name),
                branch: util::resolve_branch(ctx, sc.repository.as_ref()),
                token_env_var: Some("SCOOP_BUCKET_TOKEN".to_string()),
            });
        }
    }
    out
}

pub(crate) fn is_scoop_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.scoop.is_some()))
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "starting scoop publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.scoop` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!(
        "skipping scoop for crate '{}' — no scoop config block",
        crate_name
    )
}

/// Message emitted just before delegating to `publish_to_scoop`. Anchors
/// the scoop activity (manifest render, bucket clone, push) to a specific
/// crate in the log so multi-crate workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate scoop publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_scoop` on (not the
/// count of successful bucket pushes — `publish_to_scoop` has its own
/// skip paths for skip_upload/dry-run/etc., each of which logs its own
/// status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("finished scoop publish — {} crate(s) processed", processed)
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_scoop_per_crate_configured`
/// check passed. `selected_len` is the size of the implicit-all-resolved
/// selection. The dry-run / skip_upload paths inside `publish_to_scoop`
/// return Ok(false) without pushing — `processed` must still increment
/// for them, otherwise this predicate fires a false-positive warning even
/// though the correct code path ran.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.scoop` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.scoop`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every scoop-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "scoop publisher registered but 0 of {} effective crate(s) had a scoop \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.scoop block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for ScoopPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        anodizer_core::env_preflight::crate_universe(&ctx.config)
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.scoop.as_ref())
            .filter(|s| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    s.skip_upload.as_ref(),
                    s.if_condition.as_deref(),
                )
            })
            .flat_map(|s| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    s.repository.as_ref(),
                    Some("SCOOP_BUCKET_TOKEN"),
                )
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_scoop_per_crate_configured);
        log.status(&run_start_message(selected.len()));
        // `processed` counts crates whose configured predicate passed and
        // whose `publish_to_scoop` invocation was reached — NOT crates
        // that pushed. The dry-run / skip_upload paths inside
        // `publish_to_scoop` return Ok(false) without pushing; that's
        // still a successful run of the correct code path, so it must
        // not trigger the no-eligible-crates warning. `any_pushed` (below)
        // tracks the orthogonal "was a bucket mutated" question used
        // to gate evidence recording.
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_scoop_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            processed += 1;
            log.status(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest carries the crate's version, not the first
            // crate's (workspace per-crate independent-version mode).
            let pushed = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| publish_to_scoop(ctx, crate_name, &log),
            )?;
            if pushed {
                any_pushed = true;
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("scoop");
        if any_pushed {
            let targets = collect_scoop_run_targets(ctx);
            evidence.extra = anodizer_core::PublishEvidenceExtra::Scoop(
                anodizer_core::publish_evidence::ScoopExtra {
                    scoop_targets: targets,
                },
            );
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_scoop_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "scoop",
                "bucket clone targets",
            ));
            return Ok(());
        }
        let unique = dedup_scoop_targets(&targets);
        let env = ctx.env_source();
        let prepared: Vec<RevertTarget> = unique
            .iter()
            .map(|t| {
                let token = t
                    .token_env_var
                    .as_deref()
                    .and_then(|n| env.var(n))
                    .or_else(|| env.var("ANODIZER_GITHUB_TOKEN"))
                    .or_else(|| env.var("GITHUB_TOKEN"));
                RevertTarget {
                    target: t.target.clone(),
                    repo_url: t.repo_url.clone(),
                    branch: t.branch.clone(),
                    token,
                    private_key: None,
                    ssh_command: None,
                }
            })
            .collect();
        let env_hint = unique
            .first()
            .and_then(|t| t.token_env_var.as_deref())
            .unwrap_or("SCOOP_BUCKET_TOKEN");
        let (reverted, failed) =
            run_revert_targets_parallel(&prepared, "scoop", Some(env_hint), &log);
        log.status(&format!(
            "scoop rollback reverted {} bucket(s), {} failure(s)",
            reverted, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, PublishConfig, RepositoryConfig, ScoopConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn scoop_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                scoop: Some(ScoopConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("scoop-bucket".to_string()),
                        branch: Some("main".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn scoop_publisher_classification() {
        let p = ScoopPublisher::new();
        assert_eq!(p.name(), "scoop");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn scoop_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = ScoopPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn scoop_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("scoop");
        let p = ScoopPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("scoop")
                && m.contains("bucket clone targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn scoop_target_extra_carries_no_secret_material() {
        // Structural pin: build evidence with a populated variant and
        // assert (a) no credential-shaped keys appear AND (b) the
        // operator-public shape is preserved. The type system pins
        // the negative half — the snapshot struct has no token field
        // to land in.
        let mut e = PublishEvidence::new("scoop");
        e.extra = anodizer_core::PublishEvidenceExtra::Scoop(
            anodizer_core::publish_evidence::ScoopExtra {
                scoop_targets: vec![ScoopTarget {
                    target: "demo".into(),
                    repo_url: "https://github.com/acme/scoop-bucket.git".into(),
                    branch: Some("main".into()),
                    token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(s.contains("SCOOP_BUCKET_TOKEN"), "{s}");
        assert!(s.contains("\"target\":\"demo\""), "{s}");
        assert!(s.contains("\"branch\":\"main\""), "{s}");
    }

    #[test]
    fn commit_outcome_is_pushed() {
        assert!(util::CommitOutcome::Pushed.is_pushed());
        assert!(!util::CommitOutcome::NoChanges.is_pushed());
    }

    #[test]
    fn scoop_target_extra_roundtrips() {
        let original = vec![ScoopTarget {
            target: "demo".into(),
            repo_url: "https://github.com/acme/scoop-bucket.git".into(),
            branch: Some("main".into()),
            token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
        }];
        let extra = anodizer_core::PublishEvidenceExtra::Scoop(
            anodizer_core::publish_evidence::ScoopExtra {
                scoop_targets: original.clone(),
            },
        );
        let decoded = decode_scoop_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn scoop_collect_run_targets_walks_per_crate_config() {
        let ctx = TestContextBuilder::new()
            .crates(vec![scoop_crate("demo")])
            .build();
        let targets = collect_scoop_run_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "demo");
        assert_eq!(targets[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn scoop_effective_publish_crates_implicit_all_when_selection_empty() {
        // Regression pin for the `selected_crates = Vec::new()` failure
        // mode: the run path used to iterate the empty Vec and silently
        // skip every configured bucket. The helper now resolves to
        // implicit-all over `publish.scoop`-carrying crates.
        let ctx = TestContextBuilder::new()
            .crates(vec![
                scoop_crate("alpha"),
                scoop_crate("beta"),
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_scoop_per_crate_configured);
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn scoop_effective_publish_crates_honors_non_empty_selection() {
        let ctx = TestContextBuilder::new()
            .crates(vec![scoop_crate("alpha"), scoop_crate("beta")])
            .selected_crates(vec!["beta".to_string()])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_scoop_per_crate_configured);
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn scoop_rollback_dedups_shared_bucket() {
        // A single bucket can be configured for multiple crates;
        // dedup so the second `git revert HEAD` doesn't undo the
        // first. Mirror of homebrew_rollback_dedups_shared_tap.
        let targets = vec![
            ScoopTarget {
                target: "alpha".into(),
                repo_url: "https://github.com/acme/scoop-bucket.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
            },
            ScoopTarget {
                target: "beta".into(),
                repo_url: "https://github.com/acme/scoop-bucket.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
            },
        ];
        let unique = dedup_scoop_targets(&targets);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].target, "alpha");
    }

    // -----------------------------------------------------------------------
    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary.

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("starting scoop publish for"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("skipping scoop for crate 'demo'"), "{msg}");
        assert!(msg.contains("no scoop config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("starting per-crate scoop publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished scoop publish"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("scoop publisher registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// The no-eligible-crates warning must fire only when the iteration
    /// loop's configured-predicate filtered every selected crate out — NOT
    /// when `publish_to_scoop` returned `Ok(false)` because of dry-run /
    /// skip_upload short-circuits.
    #[test]
    fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
        // Dry-run with one configured crate: `processed` increments on
        // crate-entry (1), so warning must not fire.
        assert!(!should_warn_no_eligible(1, 1));
        // True positive: none configured.
        assert!(should_warn_no_eligible(0, 3));
        // Empty selection → no warning.
        assert!(!should_warn_no_eligible(0, 0));
        // Partial-skip → no warning.
        assert!(!should_warn_no_eligible(1, 3));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects a scoop-configured crate. Verifies the run path is wired
    /// (returns Ok). The bug-1 regression is anchored by
    /// `should_warn_no_eligible_only_fires_when_predicate_filtered_everything`.
    #[test]
    fn scoop_publisher_run_dry_run_returns_ok() {
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![scoop_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = ScoopPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        // dry-run publish_to_scoop returns false (no actual push), so
        // evidence.extra will be empty — the run path must not error.
        let _ = decode_scoop_targets(&evidence.extra);
    }

    /// When the publisher is registered (a crate has a scoop block) but the
    /// selected-crates filter excludes every scoop-configured crate, the run
    /// path must still return Ok and record no targets.
    #[test]
    fn scoop_publisher_run_no_eligible_crates_returns_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                scoop_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-scoop crate — publisher registered but
            // run path will iterate zero scoop-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = ScoopPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert!(
            evidence.primary_ref.is_none(),
            "no scoop-eligible crate selected, primary_ref must be unset"
        );
        let targets = decode_scoop_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "no scoop-eligible crate selected, targets must be empty"
        );
    }

    #[test]
    fn scoop_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![scoop_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = ScoopPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }

    /// Building a scoop bucket manifest for a Windows artifact whose `sha256`
    /// metadata is empty must bail with an actionable error. Defaulting to
    /// `""` would emit a manifest with `architecture.hash: ""`, which
    /// `scoop install` rejects (the verify step fails before the download
    /// even begins). The bail message must name the publisher, the field,
    /// and the offending artifact.
    #[test]
    fn scoop_sha256_empty_metadata_bails_with_actionable_error() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::log::{StageLogger, Verbosity};
        let mut ctx = TestContextBuilder::new()
            .crates(vec![scoop_crate("demo")])
            .build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/demo-windows-amd64.zip"),
            name: "demo-windows-amd64.zip".to_string(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "demo".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("url".to_string(), "https://example.com/x.zip".to_string());
                // sha256 deliberately missing.
                m
            },
            size: None,
        });
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err =
            super::publish_to_scoop(&mut ctx, "demo", &log).expect_err("missing sha256 must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scoop:") && msg.contains("sha256"),
            "error must name publisher + field; got: {msg}"
        );
        assert!(
            msg.contains("demo-windows-amd64.zip"),
            "error must name the offending artifact; got: {msg}"
        );
        assert!(
            msg.contains("dist/artifacts.json") || msg.contains("re-run"),
            "error must include a next-step hint; got: {msg}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_manifest() {
        let manifest = generate_manifest(
            "cfgd",
            "1.0.0",
            "https://example.com/cfgd-1.0.0-windows-amd64.zip",
            "sha256xyz",
            "Declarative config management",
            "MIT",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["architecture"]["64bit"]["hash"], "sha256xyz");
        assert_eq!(json["license"], "MIT");
    }

    #[test]
    fn test_generate_manifest_description() {
        let manifest = generate_manifest(
            "my-tool",
            "2.1.0",
            "https://example.com/my-tool-2.1.0-windows-amd64.zip",
            "deadbeef",
            "A helpful tool",
            "Apache-2.0",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["description"], "A helpful tool");
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["license"], "Apache-2.0");
        assert_eq!(
            json["architecture"]["64bit"]["url"],
            "https://example.com/my-tool-2.1.0-windows-amd64.zip"
        );
    }

    // -----------------------------------------------------------------------
    // Deep integration tests: verify manifest JSON structure
    // -----------------------------------------------------------------------

    /// Helper to build a single 64bit ArchEntry for test convenience.
    fn arch_64(url: &str, hash: &str) -> Vec<ArchEntry> {
        vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: url.to_string(),
            hash: hash.to_string(),
            wrap_in_directory: None,
        }]
    }

    #[test]
    fn test_integration_manifest_complete_json_structure() {
        let opts = ManifestOptions {
            github_slug: Some("tj-smith47/anodizer".to_string()),
            ..Default::default()
        };
        let entries = arch_64(
            "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-windows-amd64.zip",
            "aabbccdd1122334455667788",
        );
        let manifest = generate_manifest_with_opts(
            "anodizer",
            "3.2.1",
            &entries,
            "Release automation for Rust projects",
            "Apache-2.0",
            &opts,
        )
        .unwrap();

        // Parse the manifest as JSON
        let json: serde_json::Value = serde_json::from_str(&manifest)
            .unwrap_or_else(|e| panic!("manifest should be valid JSON: {e}"));

        // Verify top-level fields exist and have correct values
        assert_eq!(json["version"], "3.2.1");
        assert_eq!(json["description"], "Release automation for Rust projects");
        assert_eq!(json["homepage"], "https://github.com/tj-smith47/anodizer");
        assert_eq!(json["license"], "Apache-2.0");

        // Verify architecture.64bit structure
        let arch_64 = &json["architecture"]["64bit"];
        assert!(
            arch_64.is_object(),
            "architecture.64bit should be an object"
        );
        assert_eq!(
            arch_64["url"],
            "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-windows-amd64.zip"
        );
        assert_eq!(arch_64["hash"], "aabbccdd1122334455667788");
        // `bin` is always an array, even for a single binary.
        assert_eq!(
            arch_64["bin"],
            serde_json::json!(["anodizer.exe"]),
            "single-binary `bin` must still be a JSON array"
        );

        // checkver and autoupdate are NOT emitted.
        assert!(
            json.get("checkver").is_none(),
            "should NOT have checkver key"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "should NOT have autoupdate key"
        );
    }

    #[test]
    fn test_integration_manifest_is_valid_pretty_json() {
        let manifest = generate_manifest(
            "my-tool",
            "1.5.0",
            "https://example.com/my-tool-1.5.0-windows-amd64.zip",
            "deadbeefcafebabe",
            "A useful tool",
            "MIT",
        )
        .unwrap();

        // Verify it is pretty-printed (has newlines and indentation)
        assert!(manifest.contains('\n'), "should be pretty-printed");
        assert!(manifest.contains("  "), "should have indentation");

        // Verify it can be re-parsed
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify all expected top-level keys
        let obj = json.as_object().unwrap();
        let keys: Vec<&String> = obj.keys().collect();
        assert!(
            keys.iter().any(|k| k.as_str() == "version"),
            "should have version key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "description"),
            "should have description key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "homepage"),
            "should have homepage key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "license"),
            "should have license key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "architecture"),
            "should have architecture key"
        );
        // checkver and autoupdate are only present when github_slug is set
        assert!(
            !keys.iter().any(|k| k.as_str() == "checkver"),
            "should NOT have checkver key when github_slug is absent"
        );
        assert!(
            !keys.iter().any(|k| k.as_str() == "autoupdate"),
            "should NOT have autoupdate key when github_slug is absent"
        );
    }

    #[test]
    fn test_integration_manifest_special_characters_in_description() {
        let manifest = generate_manifest(
            "json-tool",
            "1.0.0",
            "https://example.com/tool.zip",
            "hash123",
            "A tool for \"parsing\" JSON & XML <data>",
            "MIT",
        )
        .unwrap();

        // Even with special characters, should produce valid JSON
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap_or_else(|e| {
            panic!("manifest with special chars should still be valid JSON: {e}")
        });
        assert_eq!(
            json["description"],
            "A tool for \"parsing\" JSON & XML <data>"
        );
    }

    #[test]
    fn test_integration_manifest_bin_matches_name() {
        // Verify that the bin field in the manifest matches the name parameter
        let manifest = generate_manifest(
            "my-special-cli",
            "0.1.0",
            "https://example.com/cli.zip",
            "abc",
            "desc",
            "MIT",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(
            json["architecture"]["64bit"]["bin"],
            serde_json::json!(["my-special-cli.exe"]),
            "bin should match the tool name (always an array)"
        );
    }

    #[test]
    fn test_manifest_no_autoupdate_even_with_slug() {
        // checkver/autoupdate are never emitted.
        let opts = ManifestOptions {
            github_slug: Some("myorg/release-tool".to_string()),
            ..Default::default()
        };
        let entries = arch_64(
            "https://example.com/release-tool-5.0.0-windows-amd64.zip",
            "hash",
        );
        let manifest =
            generate_manifest_with_opts("release-tool", "5.0.0", &entries, "desc", "MIT", &opts)
                .unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(
            json.get("checkver").is_none(),
            "should NOT have checkver key"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "should NOT have autoupdate key"
        );
    }

    // -----------------------------------------------------------------------
    // Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_scoop_manifest_architecture_structure() {
        let manifest = generate_manifest(
            "myapp",
            "1.0.0",
            "https://example.com/myapp-1.0.0-windows-amd64.zip",
            "deadbeef",
            "My application",
            "Apache-2.0",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify architecture.64bit has all expected fields
        let arch64 = &json["architecture"]["64bit"];
        assert_eq!(
            arch64["url"],
            "https://example.com/myapp-1.0.0-windows-amd64.zip"
        );
        assert_eq!(arch64["hash"], "deadbeef");
        assert_eq!(
            arch64["bin"],
            serde_json::json!(["myapp.exe"]),
            "single-binary `bin` must still be a JSON array"
        );
    }

    #[test]
    fn test_scoop_manifest_no_checkver_autoupdate_with_slug() {
        // checkver/autoupdate are never emitted, even with a slug.
        let opts = ManifestOptions {
            github_slug: Some("myorg/mytool".to_string()),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/mytool.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "2.0.0", &entries, "desc", "MIT", &opts).unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(
            json.get("checkver").is_none(),
            "should NOT have checkver key"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "should NOT have autoupdate key"
        );
    }

    #[test]
    fn test_scoop_manifest_no_checkver_autoupdate_without_slug() {
        let manifest = generate_manifest(
            "mytool",
            "2.0.0",
            "https://example.com/mytool.zip",
            "abc",
            "desc",
            "MIT",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(
            json.get("checkver").is_none(),
            "checkver should be absent without github_slug"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "autoupdate should be absent without github_slug"
        );
    }

    #[test]
    fn test_scoop_manifest_homepage_derived_from_name() {
        let manifest = generate_manifest(
            "my-tool",
            "1.0.0",
            "https://example.com/t.zip",
            "hash",
            "desc",
            "MIT",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://github.com/my-tool");
    }

    // -----------------------------------------------------------------------
    // New fields: homepage, persist, depends, pre/post_install, shortcuts
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_custom_homepage() {
        let opts = ManifestOptions {
            homepage: Some("https://example.com/mytool"),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://example.com/mytool");
    }

    #[test]
    fn test_manifest_homepage_fallback() {
        let manifest = generate_manifest(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://github.com/mytool");
    }

    #[test]
    fn test_manifest_persist() {
        let persist = vec!["data".to_string(), "config.ini".to_string()];
        let opts = ManifestOptions {
            persist: Some(&persist),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["persist"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "data");
        assert_eq!(arr[1], "config.ini");
    }

    #[test]
    fn test_manifest_depends() {
        let depends = vec!["git".to_string(), "7zip".to_string()];
        let opts = ManifestOptions {
            depends: Some(&depends),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["depends"].as_array().unwrap();
        assert_eq!(arr, &["git", "7zip"]);
    }

    #[test]
    fn test_manifest_pre_install() {
        let pre = vec!["Write-Host 'Installing...'".to_string()];
        let opts = ManifestOptions {
            pre_install: Some(&pre),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["pre_install"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Write-Host 'Installing...'");
    }

    #[test]
    fn test_manifest_post_install() {
        let post = vec!["Write-Host 'Done!'".to_string()];
        let opts = ManifestOptions {
            post_install: Some(&post),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["post_install"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Write-Host 'Done!'");
    }

    #[test]
    fn test_manifest_shortcuts() {
        let shortcuts = vec![
            vec!["myapp.exe".to_string(), "My App".to_string()],
            vec![
                "myapp.exe".to_string(),
                "My App CLI".to_string(),
                "--cli".to_string(),
            ],
        ];
        let opts = ManifestOptions {
            shortcuts: Some(&shortcuts),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["shortcuts"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0][0], "myapp.exe");
        assert_eq!(arr[0][1], "My App");
        assert_eq!(arr[1][2], "--cli");
    }

    #[test]
    fn test_manifest_no_optional_fields_when_not_set() {
        let manifest = generate_manifest(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(json.get("persist").is_none());
        assert!(json.get("depends").is_none());
        assert!(json.get("pre_install").is_none());
        assert!(json.get("post_install").is_none());
        assert!(json.get("shortcuts").is_none());
    }

    #[test]
    fn test_manifest_all_new_fields_together() {
        let persist = vec!["data".to_string()];
        let depends = vec!["git".to_string()];
        let pre = vec!["echo pre".to_string()];
        let post = vec!["echo post".to_string()];
        let shortcuts = vec![vec!["app.exe".to_string(), "App".to_string()]];
        let opts = ManifestOptions {
            homepage: Some("https://example.com"),
            github_slug: None,
            persist: Some(&persist),
            depends: Some(&depends),
            pre_install: Some(&pre),
            post_install: Some(&post),
            shortcuts: Some(&shortcuts),
            bin: None,
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts).unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://example.com");
        assert!(json["persist"].is_array());
        assert!(json["depends"].is_array());
        assert!(json["pre_install"].is_array());
        assert!(json["post_install"].is_array());
        assert!(json["shortcuts"].is_array());
    }

    // -----------------------------------------------------------------------
    // Multi-arch manifest tests (32bit + 64bit + arm64)
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_multi_arch_all_three() {
        let entries = vec![
            ArchEntry {
                scoop_arch: "64bit".to_string(),
                url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
                hash: "hash_amd64".to_string(),
                wrap_in_directory: None,
            },
            ArchEntry {
                scoop_arch: "32bit".to_string(),
                url: "https://example.com/app-1.0.0-windows-386.zip".to_string(),
                hash: "hash_386".to_string(),
                wrap_in_directory: None,
            },
            ArchEntry {
                scoop_arch: "arm64".to_string(),
                url: "https://example.com/app-1.0.0-windows-arm64.zip".to_string(),
                hash: "hash_arm64".to_string(),
                wrap_in_directory: None,
            },
        ];
        let opts = ManifestOptions {
            github_slug: Some("myorg/app".to_string()),
            ..Default::default()
        };
        let manifest =
            generate_manifest_with_opts("app", "1.0.0", &entries, "A multi-arch app", "MIT", &opts)
                .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify all three architecture blocks
        let arch = &json["architecture"];
        assert!(arch["64bit"].is_object(), "64bit block should exist");
        assert!(arch["32bit"].is_object(), "32bit block should exist");
        assert!(arch["arm64"].is_object(), "arm64 block should exist");

        // Verify URLs and hashes
        assert_eq!(
            arch["64bit"]["url"],
            "https://example.com/app-1.0.0-windows-amd64.zip"
        );
        assert_eq!(arch["64bit"]["hash"], "hash_amd64");
        assert_eq!(arch["64bit"]["bin"], serde_json::json!(["app.exe"]));

        assert_eq!(
            arch["32bit"]["url"],
            "https://example.com/app-1.0.0-windows-386.zip"
        );
        assert_eq!(arch["32bit"]["hash"], "hash_386");
        assert_eq!(arch["32bit"]["bin"], serde_json::json!(["app.exe"]));

        assert_eq!(
            arch["arm64"]["url"],
            "https://example.com/app-1.0.0-windows-arm64.zip"
        );
        assert_eq!(arch["arm64"]["hash"], "hash_arm64");
        assert_eq!(arch["arm64"]["bin"], serde_json::json!(["app.exe"]));

        // checkver/autoupdate are never emitted.
        assert!(
            json.get("checkver").is_none(),
            "should NOT have checkver key"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "should NOT have autoupdate key"
        );
    }

    // -----------------------------------------------------------------------
    // wrap_in_directory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_wrap_in_directory_single_bin() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
            hash: "hash123".to_string(),
            wrap_in_directory: Some("app-1.0.0".to_string()),
        }];
        let manifest = generate_manifest_with_opts(
            "app",
            "1.0.0",
            &entries,
            "An app",
            "MIT",
            &ManifestOptions::default(),
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // With wrap_in_directory, single bin becomes a pair: ["dir/bin.exe", "alias"]
        // Paths use forward slashes.
        let bin = &json["architecture"]["64bit"]["bin"];
        assert!(bin.is_array(), "bin should be an array");
        let pair = &bin[0];
        assert!(pair.is_array(), "bin entry should be a [path, alias] pair");
        assert_eq!(pair[0], "app-1.0.0/app.exe");
        assert_eq!(pair[1], "app");
    }

    #[test]
    fn test_manifest_wrap_in_directory_multiple_bins() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/suite-1.0.0.zip".to_string(),
            hash: "hash456".to_string(),
            wrap_in_directory: Some("suite-1.0.0".to_string()),
        }];
        let bins = vec!["cli".to_string(), "daemon".to_string()];
        let opts = ManifestOptions {
            bin: Some(&bins),
            ..Default::default()
        };
        let manifest =
            generate_manifest_with_opts("suite", "1.0.0", &entries, "A suite", "MIT", &opts)
                .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let bin = &json["architecture"]["64bit"]["bin"];
        assert!(bin.is_array());
        assert_eq!(bin.as_array().unwrap().len(), 2);
        assert_eq!(bin[0][0], "suite-1.0.0/cli.exe");
        assert_eq!(bin[0][1], "cli");
        assert_eq!(bin[1][0], "suite-1.0.0/daemon.exe");
        assert_eq!(bin[1][1], "daemon");
    }

    #[test]
    fn test_manifest_no_wrap_emits_bin_as_array() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/app.zip".to_string(),
            hash: "hash789".to_string(),
            wrap_in_directory: None,
        }];
        let manifest = generate_manifest_with_opts(
            "app",
            "1.0.0",
            &entries,
            "An app",
            "MIT",
            &ManifestOptions::default(),
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // Without wrap_in_directory, single-binary `bin` is still a
        // JSON array, not a bare string.
        assert_eq!(
            json["architecture"]["64bit"]["bin"],
            serde_json::json!(["app.exe"]),
            "single-binary `bin` must still be a JSON array"
        );
    }

    // -----------------------------------------------------------------------
    // skip_upload tests (reuses should_skip_upload from homebrew)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Scoop manifest name override
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_name_override() {
        // When ScoopConfig.name is set, the manifest bin and filename should
        // use the override name.
        let manifest = generate_manifest(
            "custom-name",
            "1.0.0",
            "https://example.com/custom-name-1.0.0-windows-amd64.zip",
            "abc123",
            "A custom named tool",
            "MIT",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(
            json["architecture"]["64bit"]["bin"],
            serde_json::json!(["custom-name.exe"])
        );
    }

    // -----------------------------------------------------------------------
    // Scoop manifest directory placement (dry-run test)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Scoop commit message template (uses shared render_commit_msg)
    // -----------------------------------------------------------------------

    #[test]
    fn test_scoop_commit_msg_default() {
        // Canonical default: "Scoop update for {{ .ProjectName }} version {{ .Tag }}"
        let scoop_default = "Scoop update for {{ ProjectName }} version {{ Tag }}";
        let log =
            anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
        let msg = crate::homebrew::render_commit_msg(
            Some(scoop_default),
            "mytool",
            "1.2.3",
            "manifest",
            &log,
            false,
        )
        .unwrap();
        assert_eq!(msg, "Scoop update for mytool version 1.2.3");
    }

    #[test]
    fn test_scoop_commit_msg_custom() {
        let log =
            anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
        let msg = crate::homebrew::render_commit_msg(
            Some("scoop: bump {{ name }} to {{ version }}"),
            "mytool",
            "3.0.0",
            "manifest",
            &log,
            false,
        )
        .unwrap();
        assert_eq!(msg, "scoop: bump mytool to 3.0.0");
    }

    // -----------------------------------------------------------------------
    // Multi-artifact disambiguation tests
    // -----------------------------------------------------------------------

    use anodizer_core::log::{StageLogger, Verbosity};

    fn arch_entry(scoop_arch: &str, url: &str, hash: &str) -> ArchEntry {
        ArchEntry {
            scoop_arch: scoop_arch.to_string(),
            url: url.to_string(),
            hash: hash.to_string(),
            wrap_in_directory: None,
        }
    }

    fn test_log() -> StageLogger {
        StageLogger::new("publish", Verbosity::Normal)
    }

    /// Extract the error message from a `Result<Vec<ArchEntry>>`. `.unwrap_err()`
    /// is unusable here because `ArchEntry` deliberately doesn't derive `Debug`.
    fn expect_err(result: anyhow::Result<Vec<ArchEntry>>) -> String {
        match result {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn test_disambiguate_arch_entries_single_per_arch_unchanged() {
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
                "zip".to_string(),
            ),
            (
                arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
                "zip".to_string(),
            ),
        ];
        let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
        assert_eq!(result.len(), 2);
        let amd = result
            .iter()
            .find(|e| e.scoop_arch == "64bit")
            .expect("64bit missing");
        assert_eq!(amd.url, "https://example.com/tool-amd64.zip");
        assert_eq!(amd.hash, "sha64");
        let arm = result
            .iter()
            .find(|e| e.scoop_arch == "arm64")
            .expect("arm64 missing");
        assert_eq!(arm.url, "https://example.com/tool-arm64.zip");
        assert_eq!(arm.hash, "shaarm");
    }

    #[test]
    fn test_disambiguate_arch_entries_deterministic_order() {
        // Same input must produce the same output order across runs.
        let entries = || {
            vec![
                (
                    arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
                    "zip".to_string(),
                ),
                (
                    arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
                    "zip".to_string(),
                ),
                (
                    arch_entry("32bit", "https://example.com/tool-i386.zip", "sha32"),
                    "zip".to_string(),
                ),
            ]
        };
        let r1 = disambiguate_arch_entries(entries(), false, "anodizer", &test_log()).unwrap();
        let r2 = disambiguate_arch_entries(entries(), false, "anodizer", &test_log()).unwrap();
        let keys1: Vec<&str> = r1.iter().map(|e| e.scoop_arch.as_str()).collect();
        let keys2: Vec<&str> = r2.iter().map(|e| e.scoop_arch.as_str()).collect();
        assert_eq!(keys1, keys2, "disambiguation order must be deterministic");
    }

    #[test]
    fn test_disambiguate_arch_entries_prefers_zip_over_tar_gz() {
        // 64bit appears with both .zip and .tar.gz; zip must win.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_tgz"),
                "tar.gz".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha_zip"),
                "zip".to_string(),
            ),
        ];
        let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].hash, "sha_zip", "expected zip to be selected");
    }

    #[test]
    fn test_disambiguate_arch_entries_prefers_tar_gz_when_no_zip() {
        // 64bit with tar.gz and tar.xz; tar.gz must win.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-amd64.tar.xz", "sha_xz"),
                "tar.xz".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_gz"),
                "tar.gz".to_string(),
            ),
        ];
        let result = disambiguate_arch_entries(entries, false, "anodizer", &test_log()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].hash, "sha_gz", "expected tar.gz to be selected");
    }

    #[test]
    fn test_disambiguate_arch_entries_errors_when_ids_set_and_duplicate() {
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-a.zip", "sha_a"),
                "zip".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-b.zip", "sha_b"),
                "zip".to_string(),
            ),
        ];
        let msg = expect_err(disambiguate_arch_entries(
            entries,
            true,
            "anodizer",
            &test_log(),
        ));
        assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
        assert!(
            msg.contains("crate 'anodizer'"),
            "missing crate name: {msg}"
        );
        assert!(
            msg.contains("multiple archives found for"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("tool-a.zip") && msg.contains("tool-b.zip"),
            "error must name conflicting artifacts: {msg}"
        );
    }

    #[test]
    fn test_disambiguate_arch_entries_errors_when_no_preferred_format() {
        // Two non-preferred formats for the same arch, ids unset → error.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool.tar.xz", "sha_xz"),
                "tar.xz".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool.tar.zst", "sha_zst"),
                "tar.zst".to_string(),
            ),
        ];
        let msg = expect_err(disambiguate_arch_entries(
            entries,
            false,
            "anodizer",
            &test_log(),
        ));
        assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
        assert!(
            msg.contains("crate 'anodizer'"),
            "missing crate name: {msg}"
        );
        assert!(
            msg.contains("none matches a preferred format"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("tool.tar.xz") && msg.contains("tool.tar.zst"),
            "error must name conflicting artifacts: {msg}"
        );
    }

    #[test]
    fn test_disambiguate_arch_entries_errors_when_multiple_tar_gz_no_zip() {
        // Two tar.gz archives for the same arch with no zip and ids unset.
        // Previous code path misreported this as "multiple .zip artifacts";
        // the correct error names tar.gz as the conflicting bucket.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-A.tar.gz", "sha_a"),
                "tar.gz".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-B.tar.gz", "sha_b"),
                "tar.gz".to_string(),
            ),
        ];
        let msg = expect_err(disambiguate_arch_entries(
            entries,
            false,
            "anodizer",
            &test_log(),
        ));
        assert!(msg.starts_with("scoop:"), "missing prefix: {msg}");
        assert!(
            msg.contains("multiple .tar.gz archives"),
            "expected tar.gz to be named in error, got: {msg}"
        );
        assert!(
            !msg.contains("multiple .zip"),
            "must not blame zip when there is none: {msg}"
        );
        assert!(
            msg.contains("tool-A.tar.gz") && msg.contains("tool-B.tar.gz"),
            "error must name conflicting artifacts: {msg}"
        );
    }

    #[test]
    fn test_disambiguate_arch_entries_ids_set_no_duplicates_passes() {
        // ids_was_set=true with one entry per arch — pass-through OK.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha64"),
                "zip".to_string(),
            ),
            (
                arch_entry("arm64", "https://example.com/tool-arm64.zip", "shaarm"),
                "zip".to_string(),
            ),
        ];
        let result = disambiguate_arch_entries(entries, true, "anodizer", &test_log()).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_disambiguate_arch_entries_empty_input() {
        let result = disambiguate_arch_entries(vec![], false, "anodizer", &test_log()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_disambiguate_arch_entries_logs_dropped_via_sink() {
        // Two archives for the same scoop_arch with ids unset: the fallback
        // keeps the .zip and drops the .tar.gz. Capture the warn sink to
        // assert both URLs appear in the emitted log line.
        let entries = vec![
            (
                arch_entry("64bit", "https://example.com/tool-amd64.tar.gz", "sha_tgz"),
                "tar.gz".to_string(),
            ),
            (
                arch_entry("64bit", "https://example.com/tool-amd64.zip", "sha_zip"),
                "zip".to_string(),
            ),
        ];
        let mut captured: Vec<String> = Vec::new();
        let result = crate::util::disambiguate_by_format_with_sink(
            entries,
            |(entry, _)| entry.scoop_arch.clone(),
            |(_, fmt)| fmt.as_str(),
            |(entry, _)| entry.url.clone(),
            crate::util::DisambiguateInnerConfig {
                preferred_formats: super::SCOOP_PREFERRED_FORMATS,
                ids_was_set: false,
                publisher_label: "scoop",
                crate_name: "anodizer",
            },
            &mut |msg| captured.push(msg.to_string()),
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(captured.len(), 1, "expected exactly one warn line");
        let line = &captured[0];
        assert!(
            line.starts_with("scoop:"),
            "warn line should carry publisher prefix: {line}"
        );
        assert!(
            line.contains("crate 'anodizer'"),
            "warn line should name the crate: {line}"
        );
        assert!(
            line.contains("tool-amd64.zip") && line.contains("(.zip)"),
            "warn line should name the kept archive: {line}"
        );
        assert!(
            line.contains("tool-amd64.tar.gz") && line.contains("(.tar.gz)"),
            "warn line should name the dropped archive: {line}"
        );
    }
}

// ===========================================================================
// PUBLISH FLOW — render_scoop_manifest_for_crate + publish_to_scoop's
// clone→write→commit→push→PR path, the artifact-eligibility filters, and the
// Publisher::run/rollback orchestration.
//
// The end-to-end tests drive the live publish against a local bare git repo:
// `repository.git.url` points the clone at a `file` path (no network), and the
// PR-submission transport is forced onto an in-process scripted responder by
// installing a failing `gh` stub (so `gh_is_available()` is false) and pointing
// `ANODIZER_GITHUB_API_BASE` at the responder. These tests mutate PATH +
// process env, so each is `#[cfg(unix)]` + `#[serial]`. Precedent: the krew
// publish-flow tests in this crate and `crates/stage-publish/src/npm/tests.rs`.
// ===========================================================================

#[cfg(test)]
mod publish_flow_tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, CrateConfig, GitRepoConfig, PublishConfig, ReleaseConfig, RepositoryConfig,
        ScmRepoConfig, ScoopConfig, StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use std::collections::HashMap;

    fn quiet() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    fn build_ctx(crates: Vec<CrateConfig>, version: &str) -> Context {
        let config = Config {
            crates,
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
        ctx.template_vars_mut().set("ProjectName", "widget");
        ctx
    }

    /// A scoop crate whose bucket clones from a local bare repo (`git.url`).
    /// `release.github = acme/widget` provides the homepage-slug fallback.
    fn scoop_crate_for_bucket(crate_name: &str, bucket_url: &str) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                scoop: Some(ScoopConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("scoop-bucket".to_string()),
                        branch: Some("main".to_string()),
                        token: Some("ghp_test".to_string()),
                        git: Some(GitRepoConfig {
                            url: Some(bucket_url.to_string()),
                            ssh_command: None,
                            private_key: None,
                        }),
                        ..Default::default()
                    }),
                    description: Some("Manage widgets from Windows".to_string()),
                    license: Some("MIT".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Register one Windows archive artifact carrying the `url` / `sha256` /
    /// `binary` / `format` metadata the manifest's `architecture` block reads.
    fn add_windows_archive(
        ctx: &mut Context,
        crate_name: &str,
        target: &str,
        arch: &str,
        binary: &str,
        sha: &str,
    ) {
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-windows-{arch}.zip"
            ),
        );
        meta.insert("sha256".to_string(), sha.to_string());
        meta.insert("format".to_string(), "zip".to_string());
        meta.insert("binary".to_string(), binary.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{binary}-windows-{arch}.zip")),
            name: format!("{binary}-windows-{arch}.zip"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    // -----------------------------------------------------------------
    // is_scoop_windows_artifact / ScoopArtifactFilters / crate_has_scoop
    // -----------------------------------------------------------------

    fn artifact_with(target: Option<&str>, path: &str, meta: &[(&str, &str)]) -> Artifact {
        let mut m = HashMap::new();
        for (k, v) in meta {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            target: target.map(str::to_string),
            crate_name: "widget".to_string(),
            metadata: m,
            size: None,
        }
    }

    /// Windows is detected by the target triple OR by the artifact path —
    /// either alone suffices, and a non-Windows artifact is rejected.
    #[test]
    fn is_scoop_windows_artifact_by_target_or_path() {
        assert!(is_scoop_windows_artifact(&artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w-amd64.zip",
            &[]
        )));
        // No windows in the target, but the path carries it.
        assert!(is_scoop_windows_artifact(&artifact_with(
            Some("x86_64-unknown-linux-gnu"),
            "/dist/widget-windows-amd64.zip",
            &[]
        )));
        // Neither target nor path mentions windows → not a scoop artifact.
        assert!(!is_scoop_windows_artifact(&artifact_with(
            Some("x86_64-unknown-linux-gnu"),
            "/dist/widget-linux-amd64.tar.gz",
            &[]
        )));
        // Absent target falls back to the path check (no windows here).
        assert!(!is_scoop_windows_artifact(&artifact_with(
            None,
            "/dist/widget-linux.tar.gz",
            &[]
        )));
    }

    /// A universal binary that did NOT replace single-arch variants
    /// (`replaces=false`) is filtered out before the Windows check — the
    /// `only_replacing_unibins` guard.
    #[test]
    fn scoop_filters_reject_non_replacing_unibin() {
        let cfg = ScoopConfig::default();
        let filters = ScoopArtifactFilters::from_config(&cfg);
        let a = artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w.zip",
            &[("replaces", "false")],
        );
        assert!(
            !filters.matches(&a),
            "a non-replacing universal binary must be excluded"
        );
    }

    /// The `amd64_variant` filter (default `v1`) drops an amd64 Windows
    /// artifact whose recorded variant differs, and keeps a matching one.
    #[test]
    fn scoop_filters_amd64_variant_default_v1() {
        let cfg = ScoopConfig::default(); // amd64_variant unset → defaults to v1
        let filters = ScoopArtifactFilters::from_config(&cfg);
        let v3 = artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w.zip",
            &[("amd64_variant", "v3")],
        );
        assert!(
            !filters.matches(&v3),
            "amd64_variant=v3 must be filtered when default v1 is wanted"
        );
        let v1 = artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w.zip",
            &[("amd64_variant", "v1")],
        );
        assert!(filters.matches(&v1), "amd64_variant=v1 must match default");
    }

    /// The `ids` allow-list filters by the artifact's `id` metadata: an
    /// artifact whose id is not in the list is excluded.
    #[test]
    fn scoop_filters_ids_allowlist() {
        let cfg = ScoopConfig {
            ids: Some(vec!["wanted".to_string()]),
            ..Default::default()
        };
        let filters = ScoopArtifactFilters::from_config(&cfg);
        let included = artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w.zip",
            &[("id", "wanted")],
        );
        let excluded = artifact_with(
            Some("x86_64-pc-windows-msvc"),
            "/dist/w.zip",
            &[("id", "other")],
        );
        assert!(filters.matches(&included), "id 'wanted' must match");
        assert!(!filters.matches(&excluded), "id 'other' must be excluded");
    }

    /// `crate_has_scoop_artifacts` is false on an empty set and true once an
    /// eligible Windows archive exists — the offline validator's skip signal.
    #[test]
    fn crate_has_scoop_artifacts_reflects_presence() {
        let c = scoop_crate_for_bucket("widget", "/unused");
        let scoop_cfg = c
            .publish
            .as_ref()
            .and_then(|p| p.scoop.clone())
            .expect("scoop cfg");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        assert!(
            !crate_has_scoop_artifacts(&ctx, "widget", &scoop_cfg),
            "no windows archive => not eligible"
        );
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"a".repeat(64),
        );
        assert!(
            crate_has_scoop_artifacts(&ctx, "widget", &scoop_cfg),
            "one windows archive => eligible"
        );
    }

    // -----------------------------------------------------------------
    // render_scoop_manifest_for_crate — render/skip/error boundaries.
    // -----------------------------------------------------------------

    /// `skip_upload: true` short-circuits the renderer to `None` (the
    /// publisher renders nothing for this crate) BEFORE the no-artifact
    /// guard — there are no artifacts here, yet the result is `Ok(None)`.
    #[test]
    fn render_scoop_skip_upload_true_returns_none() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.skip_upload = Some(StringOrBool::Bool(true));
        }
        let ctx = build_ctx(vec![c], "1.0.0");
        let out = render_scoop_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
        assert!(out.is_none(), "skip_upload=true must render nothing");
    }

    /// A falsy `if:` condition short-circuits the renderer to `None`.
    #[test]
    fn render_scoop_falsy_if_returns_none() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.if_condition = Some("false".to_string());
        }
        let ctx = build_ctx(vec![c], "1.0.0");
        let out = render_scoop_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
        assert!(out.is_none(), "falsy `if` must render nothing");
    }

    /// No Windows archive → hard error naming the crate.
    #[test]
    fn render_scoop_no_windows_artifact_bails() {
        let c = scoop_crate_for_bucket("widget", "/unused");
        let ctx = build_ctx(vec![c], "1.0.0");
        let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
            .expect_err("no windows archive must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("no Windows archive artifact"), "got: {msg}");
        assert!(msg.contains("widget"), "must name the crate: {msg}");
    }

    /// The rendered manifest embeds the artifact's real sha256, the
    /// metadata-`url`, the `bin` derived from the `binary` metadata, the
    /// release-github homepage slug, and the configured license — the full
    /// metadata→manifest plumbing.
    #[test]
    fn render_scoop_embeds_real_metadata() {
        let c = scoop_crate_for_bucket("widget", "/unused");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let sha = "b".repeat(64);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &sha,
        );
        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["description"], "Manage widgets from Windows");
        assert_eq!(json["license"], "MIT");
        assert_eq!(json["homepage"], "https://github.com/acme/widget");
        assert_eq!(json["architecture"]["64bit"]["hash"], sha);
        assert_eq!(
            json["architecture"]["64bit"]["url"],
            "https://github.com/acme/widget/releases/download/v1.0.0/widget-windows-amd64.zip"
        );
        assert_eq!(
            json["architecture"]["64bit"]["bin"],
            serde_json::json!(["widget.exe"]),
            "bin must derive from the `binary` metadata + .exe suffix"
        );
    }

    /// `url_template` overrides the artifact's metadata URL in the rendered
    /// manifest; the raw artifact URL must be gone. `{{ name }}` resolves to
    /// the manifest name and `{{ os }}` to `windows`.
    #[test]
    fn render_scoop_url_template_overrides_metadata_url() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.url_template = Some(
                "https://dl.acme.example/{{ name }}/{{ version }}/{{ os }}-{{ arch }}.zip"
                    .to_string(),
            );
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"a".repeat(64),
        );
        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
        assert_eq!(
            json["architecture"]["64bit"]["url"],
            "https://dl.acme.example/widget/1.0.0/windows-amd64.zip",
            "url_template must rewrite the download URL"
        );
    }

    /// A `scoop.name` override drives both the manifest body and is rendered
    /// through the template engine; the homepage falls back to it when no
    /// release-github / explicit homepage is present.
    #[test]
    fn render_scoop_name_override_used_for_bin_fallback() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        // Drop release.github so the homepage falls back to the name slug,
        // and drop the binary metadata so `bin` derives from the manifest
        // name (the override).
        c.release = None;
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.name = Some("widget-cli".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        // Archive with NO `binary` metadata → bin derives from manifest name.
        let mut meta = HashMap::new();
        meta.insert("url".to_string(), "https://example.com/w.zip".to_string());
        meta.insert("sha256".to_string(), "c".repeat(64));
        meta.insert("format".to_string(), "zip".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-windows-amd64.zip"),
            name: "widget-windows-amd64.zip".to_string(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });
        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
        assert_eq!(
            json["architecture"]["64bit"]["bin"],
            serde_json::json!(["widget-cli.exe"]),
            "no `binary` metadata → bin derives from the scoop.name override"
        );
        assert_eq!(
            json["homepage"], "https://github.com/widget-cli",
            "no release.github → homepage falls back to the name slug"
        );
    }

    // -----------------------------------------------------------------
    // publish_to_scoop — non-e2e skip / dry-run guards.
    // -----------------------------------------------------------------

    /// `skip_upload: true` on the publish path returns `Ok(false)` (no push)
    /// BEFORE the repository-resolution check — repository is None here, yet
    /// the call succeeds rather than erroring on the missing repo.
    #[test]
    fn publish_scoop_skip_upload_short_circuits_before_repo_check() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.repository = None;
            s.skip_upload = Some(StringOrBool::Bool(true));
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let pushed = publish_to_scoop(&mut ctx, "widget", &quiet())
            .expect("skip_upload must short-circuit before the repo-missing check");
        assert!(!pushed, "skip_upload path must report no push");
    }

    /// Missing repository config (and skip_upload unset) is a hard error.
    #[test]
    fn publish_scoop_missing_repository_bails() {
        let mut c = scoop_crate_for_bucket("widget", "/unused");
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.repository = None;
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let err = publish_to_scoop(&mut ctx, "widget", &quiet())
            .expect_err("missing repository must bail");
        assert!(
            format!("{err:#}").contains("no repository config"),
            "got: {err:#}"
        );
    }

    /// dry-run short-circuits before any clone/push and reports no push.
    #[test]
    fn publish_scoop_dry_run_makes_no_push() {
        let c = scoop_crate_for_bucket("widget", "/unused");
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "widget");
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"a".repeat(64),
        );
        let pushed = publish_to_scoop(&mut ctx, "widget", &quiet()).expect("dry-run ok");
        assert!(!pushed, "dry-run must not push");
    }

    // -----------------------------------------------------------------
    // publish_to_scoop — full clone→write→commit→push→PR against a local
    // bare bucket repo (gated: spawns git, mutates PATH + process env).
    // -----------------------------------------------------------------

    #[cfg(unix)]
    mod e2e {
        use super::*;
        use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig};
        use anodizer_core::test_helpers::fake_tool::{FakeToolDir, PathGuard};
        use anodizer_core::test_helpers::scripted_responder::{
            ScriptedRoute, spawn_scripted_responder,
        };
        use serial_test::serial;
        use std::path::Path;
        use std::process::Command;
        use std::sync::OnceLock;

        fn ensure_git_identity() {
            static INIT: OnceLock<()> = OnceLock::new();
            INIT.get_or_init(|| {
                // SAFETY: runs once per process under OnceLock; constants only.
                unsafe {
                    std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
                    std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
                    std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
                    std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
                    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
                }
            });
        }

        fn git_ok(dir: &Path, args: &[&str]) {
            let st = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(st.success(), "git {args:?} failed");
        }

        fn git_stdout(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// Build a bare bucket repo with one commit on `main` (the branch the
        /// publish path's clone defaults to). Returns `(url, holder)`.
        fn init_bare_bucket() -> (String, tempfile::TempDir) {
            ensure_git_identity();
            let bare = tempfile::tempdir().expect("bare tempdir");
            let seed = tempfile::tempdir().expect("seed tempdir");
            git_ok(bare.path(), &["init", "--bare", "-b", "main"]);
            git_ok(seed.path(), &["init", "-b", "main"]);
            git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
            git_ok(seed.path(), &["config", "user.name", "Test"]);
            git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
            std::fs::write(seed.path().join("README"), "bucket\n").unwrap();
            git_ok(seed.path(), &["add", "README"]);
            git_ok(seed.path(), &["commit", "-m", "seed"]);
            assert!(
                Command::new("git")
                    .args(["remote", "add", "origin"])
                    .arg(bare.path())
                    .current_dir(seed.path())
                    .status()
                    .expect("remote add")
                    .success()
            );
            git_ok(seed.path(), &["push", "-u", "origin", "main"]);
            (bare.path().to_string_lossy().into_owned(), bare)
        }

        /// A `gh` stub that exits non-zero on `--version` so
        /// `gh_is_available()` is false → the PR transport falls to the API.
        fn gh_absent() -> (FakeToolDir, PathGuard) {
            let tools = FakeToolDir::new();
            tools.tool("gh").exit(1).install();
            let guard = tools.activate();
            (tools, guard)
        }

        fn set_api_base(addr: &std::net::SocketAddr) {
            // SAFETY: env mutex held by the live PathGuard from gh_absent().
            unsafe { std::env::set_var("ANODIZER_GITHUB_API_BASE", format!("http://{addr}")) };
        }
        fn clear_api_base() {
            // SAFETY: same mutex still held by the PathGuard.
            unsafe { std::env::remove_var("ANODIZER_GITHUB_API_BASE") };
        }

        /// Enable a PR against the bucket repo so `maybe_submit_pr` runs.
        fn enable_self_pr(c: &mut CrateConfig) {
            if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut())
                && let Some(r) = s.repository.as_mut()
            {
                r.pull_request = Some(PullRequestConfig {
                    enabled: Some(true),
                    base: Some(PullRequestBaseConfig {
                        // Same-repo PR base → no cross-repo fork sync against
                        // the bare repo, and the responder sees the PR POST.
                        owner: Some("acme".to_string()),
                        name: Some("scoop-bucket".to_string()),
                        branch: Some("main".to_string()),
                    }),
                    draft: None,
                    body: None,
                });
            }
        }

        /// Full publish: clone the local bucket, write `<name>.json`, commit,
        /// push to `main`, then POST the PR via the API transport. Asserts
        /// the pushed manifest carries the real sha256 AND the PR-create POST
        /// reached the bucket repo's `/pulls`.
        #[test]
        #[serial]
        fn publish_to_scoop_pushes_manifest_and_opens_pr() {
            let (_tools, _guard) = gh_absent();
            let (bucket_url, bare) = init_bare_bucket();
            let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/acme/scoop-bucket/pulls",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
                times: Some(1),
            }]);
            set_api_base(&addr);

            let mut c = scoop_crate_for_bucket("widget", &bucket_url);
            enable_self_pr(&mut c);
            let mut ctx = build_ctx(vec![c], "1.0.0");
            let sha = "d".repeat(64);
            add_windows_archive(
                &mut ctx,
                "widget",
                "x86_64-pc-windows-msvc",
                "amd64",
                "widget",
                &sha,
            );

            let pushed = publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
            assert!(pushed, "a fresh manifest push must report pushed=true");

            // The manifest landed on main with the real sha256.
            let manifest_in_repo = git_stdout(bare.path(), &["show", "main:widget.json"]);
            let json: serde_json::Value =
                serde_json::from_str(&manifest_in_repo).expect("pushed manifest is JSON");
            assert_eq!(json["architecture"]["64bit"]["hash"], sha);
            assert_eq!(json["version"], "1.0.0");

            // The PR-create POST hit the bucket repo upstream.
            let entries = req_log.lock().unwrap();
            assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
            assert_eq!(entries[0].path, "/repos/acme/scoop-bucket/pulls");
            drop(entries);
            clear_api_base();
            drop(bare);
        }

        /// `directory:` places the manifest under a subdirectory of the
        /// bucket; the pushed file lands at `<dir>/<name>.json`.
        #[test]
        #[serial]
        fn publish_to_scoop_honors_directory_subdir() {
            let (_tools, _guard) = gh_absent();
            let (bucket_url, bare) = init_bare_bucket();
            let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/acme/scoop-bucket/pulls",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            }]);
            set_api_base(&addr);

            let mut c = scoop_crate_for_bucket("widget", &bucket_url);
            enable_self_pr(&mut c);
            if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
                s.directory = Some("bucket".to_string());
            }
            let mut ctx = build_ctx(vec![c], "1.0.0");
            add_windows_archive(
                &mut ctx,
                "widget",
                "x86_64-pc-windows-msvc",
                "amd64",
                "widget",
                &"e".repeat(64),
            );

            publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
            let tree = git_stdout(bare.path(), &["ls-tree", "-r", "--name-only", "main"]);
            assert!(
                tree.lines().any(|l| l == "bucket/widget.json"),
                "manifest must land under the configured subdirectory; tree:\n{tree}"
            );
        }

        /// Re-publishing the identical manifest finds an unchanged tree and
        /// reports `pushed=false` (NoChanges) — nothing to roll back.
        #[test]
        #[serial]
        fn publish_to_scoop_idempotent_no_changes() {
            let (_tools, _guard) = gh_absent();
            let (bucket_url, bare) = init_bare_bucket();
            let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/acme/scoop-bucket/pulls",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            }]);
            set_api_base(&addr);

            let sha = "f".repeat(64);
            let build = || {
                let mut c = scoop_crate_for_bucket("widget", &bucket_url);
                enable_self_pr(&mut c);
                let mut ctx = build_ctx(vec![c], "1.0.0");
                add_windows_archive(
                    &mut ctx,
                    "widget",
                    "x86_64-pc-windows-msvc",
                    "amd64",
                    "widget",
                    &sha,
                );
                ctx
            };

            let mut ctx1 = build();
            assert!(
                publish_to_scoop(&mut ctx1, "widget", &quiet()).expect("first publish"),
                "first publish pushes"
            );
            let mut ctx2 = build();
            assert!(
                !publish_to_scoop(&mut ctx2, "widget", &quiet()).expect("second publish"),
                "re-publishing an identical manifest must report NoChanges (pushed=false)"
            );
            clear_api_base();
            drop(bare);
        }

        /// Publisher::run end-to-end with a real push records exactly one
        /// rollback target carrying the bucket repo URL + branch (the
        /// `any_pushed` evidence gate).
        #[test]
        #[serial]
        fn scoop_publisher_run_records_rollback_target_after_push() {
            use anodizer_core::Publisher;
            let (_tools, _guard) = gh_absent();
            let (bucket_url, bare) = init_bare_bucket();
            let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/repos/acme/scoop-bucket/pulls",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            }]);
            set_api_base(&addr);

            let mut c = scoop_crate_for_bucket("widget", &bucket_url);
            enable_self_pr(&mut c);
            // `run` re-scopes each crate's version through
            // `with_published_crate_scope` → `resolve_crate_tag`, which
            // hard-errors unless a real tag matching `v{{ .Version }}` exists.
            // `hermetic_tagged_repo()` (tag `v0.1.0`) supplies one so the
            // scoped version resolves (the bucket branch is `main` either way).
            let project = crate::testing::hermetic_tagged_repo();
            let config = Config {
                crates: vec![c],
                ..Default::default()
            };
            let mut ctx = Context::new(
                config,
                ContextOptions {
                    project_root: Some(project.path().to_path_buf()),
                    ..Default::default()
                },
            );
            ctx.template_vars_mut().set("Version", "0.1.0");
            ctx.template_vars_mut().set("Tag", "v0.1.0");
            ctx.template_vars_mut().set("ProjectName", "widget");
            add_windows_archive(
                &mut ctx,
                "widget",
                "x86_64-pc-windows-msvc",
                "amd64",
                "widget",
                &"a".repeat(64),
            );

            let p = ScoopPublisher::new();
            let evidence = p.run(&mut ctx).expect("publisher.run ok");
            let targets = decode_scoop_targets(&evidence.extra);
            assert_eq!(targets.len(), 1, "one pushed bucket → one rollback target");
            assert_eq!(
                targets[0].repo_url,
                "https://github.com/acme/scoop-bucket.git"
            );
            assert_eq!(targets[0].branch.as_deref(), Some("main"));
            clear_api_base();
            drop(bare);
        }
    }
}
