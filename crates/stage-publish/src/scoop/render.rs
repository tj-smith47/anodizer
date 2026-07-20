use super::*;

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
pub(crate) fn is_scoop_windows_artifact(a: &anodizer_core::artifact::Artifact) -> bool {
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
pub(crate) struct ScoopArtifactFilters<'a> {
    ids: Option<&'a [String]>,
    amd64_variant: Option<&'a str>,
}

impl<'a> ScoopArtifactFilters<'a> {
    pub(crate) fn matches(&self, a: &anodizer_core::artifact::Artifact) -> bool {
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
    pub(crate) fn from_config(scoop_cfg: &'a anodizer_core::config::ScoopConfig) -> Self {
        ScoopArtifactFilters {
            ids: scoop_cfg.ids.as_deref(),
            amd64_variant: Some(scoop_cfg.amd64_variant.map_or("v1", |v| v.as_str())),
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

    // scoop is archive-only: reject an installer `use:` before selecting any
    // artifact so the operator gets an actionable config error, not a manifest
    // that points scoop at a payload it cannot run.
    reject_unsupported_use(scoop_cfg.use_artifact.as_deref(), crate_name)?;

    // Find all Windows Archive artifacts, applying IDs + amd64_variant filter.
    let url_template = scoop_cfg.url_template.as_deref();
    let filters = ScoopArtifactFilters::from_config(scoop_cfg);

    let artifact_kind = util::resolve_artifact_kind(scoop_cfg.use_artifact.as_deref());
    let all_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);

    let raw_arch_entries: Vec<(ArchEntry, String)> = all_artifacts
        .into_iter()
        .filter(|a| filters.matches(a))
        .map(|a| -> Result<Option<(ArchEntry, String)>> {
            let target = a.target.as_deref().unwrap_or("");
            let (_, raw_arch) = anodizer_core::target::map_target(target);

            // Scoop manifests can only key on `64bit` / `arm64` / `32bit`. Map
            // the architectures it can represent; for any other architecture
            // (riscv64, ppc64le, s390x, …) warn-and-skip THIS entry rather than
            // mislabeling it as `64bit` (which would have `scoop install`
            // download an incompatible archive on x64 hosts) or hard-failing the
            // whole manifest (which would block the valid x86_64 entry). The
            // autoupdate / extract_dir blocks are derived from the surviving
            // entries below, so omitting this entry leaves no dangling reference.
            let scoop_arch = match raw_arch.as_str() {
                "amd64" => "64bit",
                "386" => "32bit",
                "arm64" => "arm64",
                other => {
                    log.warn(&format!(
                        "skipped scoop artifact '{}' for '{}' — arch '{}' has no \
                         scoop architecture key (scoop supports only \
                         64bit/arm64/32bit); omitting it from the manifest",
                        a.name(),
                        crate_name,
                        other
                    ));
                    return Ok(None);
                }
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

            Ok(Some((
                ArchEntry {
                    scoop_arch: scoop_arch.to_string(),
                    url,
                    hash,
                    wrap_in_directory,
                },
                format,
            )))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

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

    // checkver: explicit override → `"github"` when the repo is known →
    // None (no autoupdate possible without a release source). ScoopInstaller/
    // Main requires checkver+autoupdate for automated-update PRs.
    let checkver = match scoop_cfg.checkver.as_deref().filter(|s| !s.is_empty()) {
        Some(c) => Some(c.to_string()),
        None if github_slug.is_some() => Some("github".to_string()),
        None => None,
    };

    // autoupdate.hash mode mirrors what the checksum stage actually emits:
    // split mode → per-asset `<asset>.sha256` sidecars (`$url.sha256`);
    // combined mode → the single `checksums.txt` file + a per-asset regex.
    // Only build an autoupdate block when checkver is resolvable (no release
    // source ⇒ no auto-bump target).
    let autoupdate_hash = if checkver.is_some() {
        Some(resolve_autoupdate_hash(
            ctx,
            crate_cfg,
            crate_name,
            &version,
            &arch_entries,
        )?)
    } else {
        None
    };

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
    // Template-render every user-supplied string field, same warn-vs-strict
    // path the neighbouring `description` / `name` / `homepage` fields use, so a
    // value like `persist: "{{ .Tag }}"` resolves instead of shipping the literal
    // delimiter. Per-crate Tag/Version scoping is inherited because each render
    // goes through the same `ctx` the homepage render does — correct under
    // single-crate, workspace-lockstep, and workspace per-crate modes alike.
    let persist = render_string_list(ctx, log, "scoop.persist", scoop_cfg.persist.as_deref())?;
    let depends = render_string_list(ctx, log, "scoop.depends", scoop_cfg.depends.as_deref())?;
    let pre_install = render_string_list(
        ctx,
        log,
        "scoop.pre_install",
        scoop_cfg.pre_install.as_deref(),
    )?;
    let post_install = render_string_list(
        ctx,
        log,
        "scoop.post_install",
        scoop_cfg.post_install.as_deref(),
    )?;
    let shortcuts = render_shortcuts(ctx, log, scoop_cfg.shortcuts.as_deref())?;

    let opts = ManifestOptions {
        homepage: homepage_rendered.as_deref(),
        github_slug,
        persist: persist.as_deref(),
        depends: depends.as_deref(),
        pre_install: pre_install.as_deref(),
        post_install: post_install.as_deref(),
        shortcuts: shortcuts.as_deref(),
        bin: bin_names_ref,
        checkver,
        autoupdate_hash,
    };

    let manifest = generate_manifest_with_opts(
        manifest_name,
        &version,
        &arch_entries,
        &description,
        &license,
        &opts,
    )?;

    // Final-text chokepoint shared by the live publish path and the offline
    // prepublish guard (both reach the manifest string only through here): a
    // residual `{{ … }}` means a config field escaped rendering — fail strict,
    // warn lenient, before the manifest is written or pushed.
    crate::util::guard_no_unrendered(ctx, log, "scoop manifest", &manifest)?;

    Ok(Some(manifest))
}

/// Template-render each element of an optional scoop string-list field
/// (`persist` / `depends` / `pre_install` / `post_install`), preserving order
/// and length, via the same warn-vs-strict path the scalar scoop fields use.
fn render_string_list(
    ctx: &Context,
    log: &StageLogger,
    field: &str,
    list: Option<&[String]>,
) -> Result<Option<Vec<String>>> {
    match list {
        None => Ok(None),
        Some(items) => {
            let rendered = items
                .iter()
                .map(|item| util::render_or_warn(ctx, log, field, item))
                .collect::<Result<Vec<String>>>()?;
            Ok(Some(rendered))
        }
    }
}

/// Template-render scoop `shortcuts` — a list of `[exe, name, args?, icon?]`
/// tuples — rendering every element of every tuple so a templated executable
/// path, name, or argument resolves before it reaches the manifest.
fn render_shortcuts(
    ctx: &Context,
    log: &StageLogger,
    shortcuts: Option<&[Vec<String>]>,
) -> Result<Option<Vec<Vec<String>>>> {
    match shortcuts {
        None => Ok(None),
        Some(rows) => {
            let rendered = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|field| util::render_or_warn(ctx, log, "scoop.shortcuts", field))
                        .collect::<Result<Vec<String>>>()
                })
                .collect::<Result<Vec<Vec<String>>>>()?;
            Ok(Some(rendered))
        }
    }
}
