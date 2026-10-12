use super::*;

/// Template-rendered string fields that feed [`WingetManifestParams`].
/// Each field mirrors the same-named winget config entry after running
/// it through the template engine with the standard variable set plus
/// `Changelog` as an extra field.
pub(crate) struct RenderedWingetFields {
    publisher: String,
    publisher_url: Option<String>,
    publisher_support_url: Option<String>,
    privacy_url: Option<String>,
    homepage: Option<String>,
    author: Option<String>,
    copyright: Option<String>,
    copyright_url: Option<String>,
    license: String,
    license_url: Option<String>,
    short_description: String,
    release_notes_url: Option<String>,
    installation_notes: Option<String>,
    path: Option<String>,
    package_name: Option<String>,
    release_notes: Option<String>,
}

/// Template-render all 18 winget config string fields against the live
/// context, injecting `Changelog` as an extra field per render.
///
/// Each field renders strict-aware via [`util::render_or_warn_with_vars`]: a
/// malformed field template errors under the guard / `--strict`, else warns
/// and falls back to its raw value.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_winget_fields(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    crate_name: &str,
    name: &str,
    publisher_name: &str,
    license: &str,
    short_desc: &str,
    log: &StageLogger,
) -> Result<RenderedWingetFields> {
    let render = |field: &str, s: Option<&str>| render_winget_field(ctx, log, field, s);

    Ok(RenderedWingetFields {
        publisher: render("winget.publisher", Some(publisher_name))?
            .unwrap_or_else(|| publisher_name.to_string()),
        publisher_url: render("winget.publisher_url", winget_cfg.publisher_url.as_deref())?,
        publisher_support_url: render(
            "winget.publisher_support_url",
            winget_cfg.publisher_support_url.as_deref(),
        )?,
        privacy_url: render("winget.privacy_url", winget_cfg.privacy_url.as_deref())?,
        homepage: render(
            "winget.homepage",
            winget_cfg
                .homepage
                .as_deref()
                .or_else(|| ctx.config.meta_homepage_for(crate_name)),
        )?,
        author: render("winget.author", winget_cfg.author.as_deref())?,
        copyright: render("winget.copyright", winget_cfg.copyright.as_deref())?,
        copyright_url: render("winget.copyright_url", winget_cfg.copyright_url.as_deref())?,
        license: render("winget.license", Some(license))?.unwrap_or_else(|| license.to_string()),
        license_url: render("winget.license_url", winget_cfg.license_url.as_deref())?,
        short_description: render("winget.short_description", Some(short_desc))?
            .unwrap_or_else(|| short_desc.to_string())
            .replace('\t', "  "),
        release_notes_url: render(
            "winget.release_notes_url",
            winget_cfg.release_notes_url.as_deref(),
        )?,
        installation_notes: render(
            "winget.installation_notes",
            winget_cfg.installation_notes.as_deref(),
        )?,
        path: render("winget.path", winget_cfg.path.as_deref())?,
        package_name: render("winget.package_name", winget_cfg.package_name.as_deref())?
            .or_else(|| Some(name.to_string())),
        release_notes: render("winget.release_notes", winget_cfg.release_notes.as_deref())?,
    })
}

/// Template-render one winget config string field against the live context,
/// with `Changelog` (the release notes) available to the template.
///
/// Strict-aware via [`util::render_or_warn_with_vars`]: a malformed field
/// template errors under the guard / `--strict`, else warns and falls back to
/// its raw value.
pub(crate) fn render_winget_field(
    ctx: &Context,
    log: &StageLogger,
    field: &str,
    raw: Option<&str>,
) -> Result<Option<String>> {
    raw.map(|v| {
        let mut vars = ctx.template_vars().clone();
        let release_notes = vars.get("ReleaseNotes").cloned().unwrap_or_default();
        vars.set("Changelog", &release_notes);
        util::render_or_warn_with_vars(&vars, log, field, v, ctx.render_is_strict())
    })
    .transpose()
}

/// Render and check every `additional_locales[]` entry, returning each
/// locale with its rendered overrides in config order.
///
/// Runs before any manifest is generated: an entry whose locale renders
/// empty, equals the default locale, or repeats an earlier entry is an entry
/// skip for the crate, so a bad translation never leaves a partial manifest
/// set on the fork.
pub(crate) fn resolve_additional_locales(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    default_locale: &str,
    log: &StageLogger,
) -> Result<Vec<(String, WingetLocaleOverrides)>> {
    let entries = winget_cfg.additional_locales.as_deref().unwrap_or(&[]);
    let mut out: Vec<(String, WingetLocaleOverrides)> = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let key = |name: &str| format!("winget.additional_locales[{i}].{name}");
        let locale =
            render_winget_field(ctx, log, &key("locale"), Some(&entry.locale))?.unwrap_or_default();
        if locale.is_empty() {
            anyhow::bail!(anodizer_core::pipe_skip::entry_skip(format!(
                "additional_locales[{i}].locale is empty"
            )));
        }
        if locale == default_locale {
            anyhow::bail!(anodizer_core::pipe_skip::entry_skip(format!(
                "additional_locales[{i}].locale '{locale}' equals default_locale"
            )));
        }
        if out.iter().any(|(seen, _)| *seen == locale) {
            anyhow::bail!(anodizer_core::pipe_skip::entry_skip(format!(
                "additional_locales[{i}].locale '{locale}' is already listed"
            )));
        }
        let field = |name: &str, raw: Option<&str>| render_winget_field(ctx, log, &key(name), raw);
        let overrides = WingetLocaleOverrides {
            publisher: field("publisher", entry.publisher.as_deref())?,
            publisher_url: field("publisher_url", entry.publisher_url.as_deref())?,
            publisher_support_url: field(
                "publisher_support_url",
                entry.publisher_support_url.as_deref(),
            )?,
            privacy_url: field("privacy_url", entry.privacy_url.as_deref())?,
            author: field("author", entry.author.as_deref())?,
            package_name: field("package_name", entry.package_name.as_deref())?,
            homepage: field("homepage", entry.homepage.as_deref())?,
            license: field("license", entry.license.as_deref())?,
            license_url: field("license_url", entry.license_url.as_deref())?,
            copyright: field("copyright", entry.copyright.as_deref())?,
            copyright_url: field("copyright_url", entry.copyright_url.as_deref())?,
            short_description: field("short_description", entry.short_description.as_deref())?
                .map(|s| s.replace('\t', "  ")),
            description: field("description", entry.description.as_deref())?,
            tags: entry.tags.clone(),
            release_notes: field("release_notes", entry.release_notes.as_deref())?,
            release_notes_url: field("release_notes_url", entry.release_notes_url.as_deref())?,
            installation_notes: field("installation_notes", entry.installation_notes.as_deref())?,
        };
        out.push((locale, overrides));
    }
    Ok(out)
}

/// Compute the on-disk manifest directory inside the cloned winget repo
/// and write the manifest files: the version, installer and default locale
/// manifests plus one `.locale.<locale>.yaml` per additional locale.
/// Returns the directory for logging.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_winget_manifests_to_disk(
    repo_path: &std::path::Path,
    package_id: &str,
    version: &str,
    path_rendered: Option<&str>,
    default_locale: &str,
    ver_yaml: &str,
    inst_yaml: &str,
    locale_yaml: &str,
    additional_locales: &[(String, String)],
) -> Result<std::path::PathBuf> {
    let manifest_dir = if let Some(path) = path_rendered {
        repo_path.join(path)
    } else {
        let first_char = package_id
            .chars()
            .next()
            .unwrap_or('_')
            .to_ascii_lowercase();
        repo_path
            .join("manifests")
            .join(first_char.to_string())
            .join(package_id.replace('.', "/"))
            .join(version)
    };
    std::fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("winget: create manifest dir {}", manifest_dir.display()))?;

    let ver_path = manifest_dir.join(format!("{}.yaml", package_id));
    let inst_path = manifest_dir.join(format!("{}.installer.yaml", package_id));
    let locale_path = manifest_dir.join(format!("{}.locale.{}.yaml", package_id, default_locale));

    std::fs::write(&ver_path, ver_yaml)?;
    std::fs::write(&inst_path, inst_yaml)?;
    std::fs::write(&locale_path, locale_yaml)?;
    for (locale, yaml) in additional_locales {
        std::fs::write(
            manifest_dir.join(format!("{}.locale.{}.yaml", package_id, locale)),
            yaml,
        )?;
    }

    Ok(manifest_dir)
}

/// Submit (or update) the PR against either a configured `pull_request`
/// upstream or the canonical `microsoft/winget-pkgs` fallback. Returns
/// the optional outcome that must be forwarded to
/// `Context::record_publisher_outcome`.
#[allow(clippy::too_many_arguments)]
#[must_use = "the returned outcome must be forwarded to Context::record_publisher_outcome"]
pub(crate) fn submit_winget_pr(
    repo_path: &std::path::Path,
    repo_for_pr: Option<&anodizer_core::config::RepositoryConfig>,
    repo_owner: &str,
    repo_name: &str,
    branch_name: &str,
    package_id: &str,
    version: &str,
    update_existing_pr: bool,
    log: &StageLogger,
    render: &dyn Fn(&str) -> Result<String>,
    env: &dyn anodizer_core::EnvSource,
) -> Result<Option<anodizer_core::PublisherOutcome>> {
    let has_pr_config = repo_for_pr
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    let title = format!("New version: {} version {}", package_id, version);
    let body = format!(
        "## Package\n- **Package**: {}\n- **Version**: {}\n\n{}",
        package_id,
        version,
        crate::util::SUBMITTED_BY_FOOTER
    );

    if has_pr_config {
        util::maybe_submit_pr_with_env(
            repo_path,
            repo_for_pr,
            &util::PrOrigin {
                repo_owner,
                repo_name,
                branch_name,
                update_existing_pr,
            },
            &title,
            &body,
            "winget",
            log,
            render,
            env,
        )
    } else {
        // A templated `base.owner` / `base.name` must render before it forms
        // the upstream PR slug sent to the GitHub API.
        let upstream_slug = repo_for_pr
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| {
                // The base slug keeps its text when it fails to render, the
                // way `maybe_submit_pr` treats its base branch.
                let render = |s: &str| render(s).unwrap_or_else(|_| s.to_string());
                let owner = render(base.owner.as_deref()?);
                let name = render(base.name.as_deref()?);
                Some(format!("{}/{}", owner, name))
            })
            .unwrap_or_else(|| "microsoft/winget-pkgs".to_string());

        Ok(util::submit_pr_via_gh_with_opts_with_env(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &title,
            &body,
            "winget",
            log,
            util::SubmitPrOpts {
                update_existing_pr,
                token: util::configured_pr_token(repo_for_pr, render)?,
            },
            env,
        ))
    }
}

// ---------------------------------------------------------------------------
// publish_to_winget
// ---------------------------------------------------------------------------

/// The side-effect-free product of rendering a crate's WinGet manifests: the
/// three YAML documents plus the resolved identity fields the downstream
/// commit/PR steps need. Produced by [`render_winget_manifests_for_crate`] so
/// the live publish path and the offline schema validator render from one
/// source of truth.
pub(crate) struct RenderedWingetManifests {
    /// Version manifest YAML (the `<PackageIdentifier>.yaml` file).
    pub(crate) version_yaml: String,
    /// Installer manifest YAML (the `<PackageIdentifier>.installer.yaml` file).
    pub(crate) installer_yaml: String,
    /// Locale manifest YAML (the `<PackageIdentifier>.locale.<locale>.yaml` file).
    pub(crate) locale_yaml: String,
    /// One `(locale, YAML)` per `additional_locales[]` entry, in config
    /// order; each is the `<PackageIdentifier>.locale.<locale>.yaml` file of
    /// that locale.
    pub(crate) additional_locale_yaml: Vec<(String, String)>,
    /// Resolved manifest locale (default `en-US`); also names the locale
    /// manifest file.
    pub(crate) default_locale: String,
    /// Resolved fork repository owner the manifests are pushed under.
    pub(crate) repo_owner: String,
    /// Resolved fork repository name the manifests are pushed under.
    pub(crate) repo_name: String,
    /// Resolved WinGet `PackageIdentifier`.
    pub(crate) package_id: String,
    /// Crate path-rendering override (`winget.path`), already template-rendered.
    pub(crate) path: Option<String>,
}

/// The publisher's resolved identity for a crate: the package coordinates and
/// fork-repo target, derived before any manifest content is rendered. Shared by
/// the dry-run short-circuit (which only needs the coordinates to log) and the
/// full manifest render.
pub(crate) struct WingetIdentity {
    pub(crate) repo_owner: String,
    pub(crate) repo_name: String,
    pub(crate) name: String,
    pub(crate) publisher_name: String,
    pub(crate) package_id: String,
    /// The rendered `default_locale` (`en-US` when unset).
    pub(crate) default_locale: String,
    /// The rendered `additional_locales` entries, each checked against the
    /// default locale and its siblings.
    pub(crate) additional_locales: Vec<(String, WingetLocaleOverrides)>,
}

/// Resolve a crate's WinGet identity (repo, name, publisher, validated
/// `PackageIdentifier`, the locales) from its DERIVED `publish.winget` block
/// (see [`super::identifier::derive_winget_config`]), or `Ok(None)` when the
/// publisher would skip the crate (`skip_upload` / a falsy `if`).
///
/// The locales are resolved here rather than in the manifest render so an
/// entry that disqualifies the crate (an empty, duplicate or default-equal
/// `locale`) is reported before the dry-run return, which never renders a
/// manifest.
pub(crate) fn resolve_winget_identity(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    log: &StageLogger,
) -> Result<Option<WingetIdentity>> {
    let label = format!("winget publisher for crate '{}'", crate_name);
    if crate::util::should_skip_publisher_with_if(
        ctx,
        None,
        winget_cfg.skip_upload.as_ref(),
        winget_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let (repo_owner, repo_name) =
        crate::util::resolve_repo_owner_name(winget_cfg.repository.as_ref()).ok_or_else(|| {
            anodizer_core::pipe_skip::entry_skip(
                crate::publisher_helpers::MISSING_REPOSITORY_REASON,
            )
        })?;

    let name_raw = winget_cfg.name.as_deref().unwrap_or(crate_name);
    let name = util::render_or_warn(ctx, log, "winget.name", name_raw)?;
    let publisher_name =
        resolve_winget_publisher_name(winget_cfg, &repo_owner, crate_name, log)?.to_string();

    // The derived config already carries the identifier the submission uses,
    // so an identifier that renders invalid is reported as the text winget
    // would receive, not as the template.
    let package_id = super::identifier::package_identifier_of(winget_cfg);

    validate_package_identifier(&package_id)
        .map_err(|e| anodizer_core::pipe_skip::entry_skip(e.to_string()))?;

    // Manifest locale: templated, defaults to en-US.
    let default_locale_raw = winget_cfg
        .default_locale
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("en-US");
    let default_locale =
        crate::util::render_or_warn(ctx, log, "winget.default_locale", default_locale_raw)?;
    let additional_locales = resolve_additional_locales(ctx, winget_cfg, &default_locale, log)?;

    Ok(Some(WingetIdentity {
        repo_owner,
        repo_name,
        name,
        publisher_name,
        package_id,
        default_locale,
        additional_locales,
    }))
}

/// Resolve a crate's WinGet config and render its three manifests in-memory,
/// with no disk, clone, or network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip_upload`
/// or a falsy `if` condition). Errors when the crate carries no `winget` block.
/// The live publish path and the offline schema validator both call this so the
/// validated documents are byte-for-byte what a real publish would push.
pub(crate) fn render_winget_manifests_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<RenderedWingetManifests>> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;
    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?;
    let derived = super::identifier::derive_winget_config(ctx, log, winget_cfg, crate_name)?;

    let Some(identity) = resolve_winget_identity(ctx, crate_name, &derived, log)? else {
        return Ok(None);
    };
    Ok(Some(render_winget_manifests_with_identity(
        ctx, crate_name, &derived, &identity, log,
    )?))
}

/// Render a crate's three WinGet manifests from a pre-resolved
/// [`WingetIdentity`].
///
/// Split out so the live publish path can reuse the identity it already
/// resolved (for the dry-run short-circuit) rather than re-resolving it —
/// re-resolution would re-emit `resolve_winget_publisher_name`'s
/// fallback-to-repo-owner warning a second time per publish.
pub(crate) fn render_winget_manifests_with_identity(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    identity: &WingetIdentity,
    log: &StageLogger,
) -> Result<RenderedWingetManifests> {
    let name = identity.name.as_str();
    let publisher_name = identity.publisher_name.as_str();
    let package_id = identity.package_id.as_str();

    let version = ctx.version();
    let description = resolve_winget_description(ctx, winget_cfg, crate_name, log)?;
    let short_desc = resolve_winget_short_description(ctx, winget_cfg, crate_name)?;
    let license = resolve_winget_license(ctx, winget_cfg, crate_name)?;

    let installers = collect_winget_installers(ctx, crate_name, winget_cfg, name, &version, log)?;
    let product_code = resolve_winget_product_code(ctx, crate_name, winget_cfg);

    let deps = winget_cfg.dependencies.as_deref().unwrap_or(&[]);
    let release_date = resolve_winget_release_date(ctx);
    let release_date_ref = release_date.as_deref();

    let moniker = resolve_winget_moniker(ctx, crate_name, winget_cfg);
    // winget upgrade behavior: default `install` (correct for portable-zip
    // tools); `uninstallPrevious` forces a clobbering reinstall.
    let upgrade_behavior = winget_cfg
        .upgrade_behavior
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("install");
    let documentations = winget_cfg.documentations.as_deref().unwrap_or(&[]);
    let default_locale = identity.default_locale.clone();
    let additional_locales = &identity.additional_locales;

    let rendered = render_winget_fields(
        ctx,
        winget_cfg,
        crate_name,
        name,
        publisher_name,
        license,
        &short_desc,
        log,
    )?;

    let params = WingetManifestParams {
        package_id,
        name,
        package_name: rendered.package_name.as_deref(),
        version: &version,
        description: &description,
        short_description: &rendered.short_description,
        license: &rendered.license,
        license_url: rendered.license_url.as_deref(),
        publisher: &rendered.publisher,
        publisher_url: rendered.publisher_url.as_deref(),
        publisher_support_url: rendered.publisher_support_url.as_deref(),
        privacy_url: rendered.privacy_url.as_deref(),
        author: rendered.author.as_deref(),
        copyright: rendered.copyright.as_deref(),
        copyright_url: rendered.copyright_url.as_deref(),
        homepage: rendered.homepage.as_deref(),
        release_notes: rendered.release_notes.as_deref(),
        release_notes_url: rendered.release_notes_url.as_deref(),
        installation_notes: rendered.installation_notes.as_deref(),
        tags: winget_cfg.tags.as_deref(),
        dependencies: deps,
        installers,
        product_code: product_code.as_deref(),
        release_date: release_date_ref,
        moniker: moniker.as_deref(),
        upgrade_behavior,
        default_locale: &default_locale,
        documentations,
    };
    let (version_yaml, installer_yaml, locale_yaml) = generate_manifests(&params)?;
    let additional_locale_yaml = additional_locales
        .iter()
        .map(|(locale, overrides)| {
            Ok((
                locale.clone(),
                generate_locale_manifest(&params, locale, overrides)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(RenderedWingetManifests {
        version_yaml,
        installer_yaml,
        locale_yaml,
        additional_locale_yaml,
        default_locale,
        repo_owner: identity.repo_owner.clone(),
        repo_name: identity.repo_name.clone(),
        package_id: package_id.to_string(),
        path: rendered.path,
    })
}
