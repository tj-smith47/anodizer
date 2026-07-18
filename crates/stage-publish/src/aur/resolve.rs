use super::*;

// ---------------------------------------------------------------------------
// Default resolution
// ---------------------------------------------------------------------------

/// Resolved AUR `Default()`-time fields: conflicts, provides, and pkgrel.
/// Extracted from `publish_to_aur` so the defaults can be exercised in
/// unit tests without standing up a full publish-to-git flow:
///
/// - `name` raw default is computed by `aur_default_package_name`
///   (`<crate_name>` with `-bin` suffix appended when the crate name does
///   not already end in `-bin`); the caller renders templates and feeds
///   the rendered string into `aur_resolve_defaults` so `base_name` is
///   derived from the post-template name.
/// - `conflicts` defaults to `[base_name]` when unset/empty.
/// - `provides` defaults to `[base_name]` when unset/empty.
/// - `pkgrel` defaults to `1` when unset.
///
/// `base_name` is the project name when set, otherwise the rendered package
/// name with any trailing `-bin` stripped (covers the edge case where
/// `package_name="foo-bin"` and `project_name="foo-cli"`).
pub(crate) struct AurResolvedDefaults {
    pub(crate) conflicts: Vec<String>,
    pub(crate) provides: Vec<String>,
    pub(crate) pkgrel: u32,
}

/// Compute the raw (pre-template) default `aur.name`: the explicit
/// `aur_cfg.name` if Some, otherwise `<crate_name>-bin` (without
/// double-suffixing when the crate already ends in `-bin`).
///
/// This is split out from `aur_resolve_defaults` so the caller can render
/// the result through the template engine *before* `base_name` is derived
/// — otherwise `aur.name = "{{ .ProjectName }}-bin"` with an empty
/// `project_name` would carry unrendered template syntax into
/// `conflicts`/`provides`.
pub(crate) fn aur_default_package_name(
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
) -> String {
    aur_cfg.name.clone().unwrap_or_else(|| {
        if crate_name.ends_with("-bin") {
            crate_name.to_string()
        } else {
            format!("{}-bin", crate_name)
        }
    })
}

/// Apply the `Default()` rules for `conflicts`, `provides`, and
/// `pkgrel`, given a `rendered_package_name` (post-template) and a
/// `project_name` (use `""` when no project name is configured). The
/// returned struct holds the post-default values that `publish_to_aur`
/// would feed into PKGBUILD generation.
///
/// `rendered_package_name` must be the template-rendered output of
/// `aur_default_package_name` — the helper is intentionally template-free
/// so it stays pure (no `Context` dependency).
pub(crate) fn aur_resolve_defaults(
    aur_cfg: &anodizer_core::config::AurConfig,
    rendered_package_name: &str,
    project_name: &str,
) -> AurResolvedDefaults {
    let base_name = if project_name.is_empty() {
        rendered_package_name
            .strip_suffix("-bin")
            .unwrap_or(rendered_package_name)
            .to_string()
    } else {
        project_name.to_string()
    };

    let conflicts = if aur_cfg.conflicts.as_ref().is_none_or(|v| v.is_empty()) {
        vec![base_name.clone()]
    } else {
        aur_cfg.conflicts.clone().unwrap_or_default()
    };
    let provides = if aur_cfg.provides.as_ref().is_none_or(|v| v.is_empty()) {
        vec![base_name.clone()]
    } else {
        aur_cfg.provides.clone().unwrap_or_default()
    };

    let pkgrel: u32 = aur_cfg
        .rel
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    AurResolvedDefaults {
        conflicts,
        provides,
        pkgrel,
    }
}

// ---------------------------------------------------------------------------
// publish_to_aur — per-section helpers
// ---------------------------------------------------------------------------

/// Owned, post-default field set fed into `PkgbuildParams`. Built once
/// by [`aur_resolve_fields`] from the active `aur:` config + project
/// metadata fallbacks so the orchestrator stays linear.
pub(crate) struct AurResolvedFields {
    pub(crate) package_name: String,
    pub(crate) version: String,
    pub(crate) pkgrel: u32,
    pub(crate) description: String,
    /// Rendered pacman `license=()` array entries; see [`aur_license_array`].
    pub(crate) license: Vec<String>,
    pub(crate) url: String,
    pub(crate) maintainers: Vec<String>,
    pub(crate) contributors: Vec<String>,
    pub(crate) depends: Vec<String>,
    pub(crate) optdepends: Vec<String>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) provides: Vec<String>,
    pub(crate) replaces: Vec<String>,
    pub(crate) backup: Vec<String>,
}

/// Resolve the AUR push remote for the binary publisher: an explicit
/// `aur.git_url` is a verbatim override; otherwise derive the canonical
/// `ssh://aur@aur.archlinux.org/<package>.git` from the resolved package
/// name (rendered the same way the PKGBUILD path renders `aur.name`), so the
/// push target tracks `pkgbase`/`pkgname` and cannot drift. A broken
/// `aur.name` template falls back to the raw value here and is surfaced
/// (once) by the downstream PKGBUILD render.
pub(crate) fn aur_resolve_push_git_url(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<String> {
    match aur_cfg.git_url.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(url) => Ok(url.to_string()),
        None => {
            let raw_name = aur_default_package_name(aur_cfg, crate_name);
            let package_name = util::render_or_warn(ctx, log, "aur.name", &raw_name)?;
            Ok(crate::util::aur_default_git_url(&package_name))
        }
    }
}

/// Evaluate the early-exit gates (`skip`, `skip_upload`, dry-run) for the
/// AUR publisher and resolve the push `git_url`.
///
/// Returns `Ok(Some(git_url))` when the caller should proceed with
/// the publish; `Ok(None)` when an early-exit fired (the helper has
/// already emitted any operator-facing log line). Errors propagate
/// unchanged (e.g. the `skip` Tera render failure).
pub(crate) fn aur_check_skip_and_resolve_git_url(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    if let Some(ref d) = aur_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("aur: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "skipped aur for '{}' — skip evaluates true",
                crate_name
            ));
            return Ok(None);
        }
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        aur_cfg.if_condition.as_deref(),
        &format!("aur publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped aur for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(None);
    }

    if crate::util::should_skip_upload(
        aur_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("aur for '{crate_name}'")),
    )? {
        return Ok(None);
    }

    let git_url = aur_resolve_push_git_url(ctx, aur_cfg, crate_name, log)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push AUR PKGBUILD for '{}' to {}",
            crate_name, git_url
        ));
        return Ok(None);
    }

    Ok(Some(git_url))
}

/// Resolve all PKGBUILD field defaults (name, version, pkgrel, url,
/// license, dependency arrays, etc.). `crate_cfg` is consulted for the
/// `release.github` fallback when `aur.homepage` / `metadata.homepage`
/// are both unset; the AUR-default `conflicts`/`provides`/`pkgrel`
/// rules are applied via `aur_resolve_defaults` against the rendered
/// package name (so `aur.name = "{{ .ProjectName }}-bin"` does not
/// leak unrendered template syntax into the array fields).
/// Build the extra `package()` install lines (LICENSE, man pages, shell
/// completions) the AUR -bin package should install beyond the binary, per
/// Arch packaging guidelines and the zoxide-bin/starship-bin exemplars.
///
/// Each line is wrapped in a bash existence guard against `$srcdir`, because
/// the binary tarball *may* bundle a LICENSE/man/completion (the archive stage
/// auto-includes `LICENSE*` and installs completions/man under their default
/// dirs), but a crate that ships none must not produce a failing `install`.
/// The guard keeps the body `bash -n`-clean and namcap-quiet either way.
///
/// Arch destination conventions (mirrored from real -bin packages):
/// - LICENSE      → `/usr/share/licenses/$pkgname/LICENSE` (REQUIRED)
/// - man (sec. 1) → `/usr/share/man/man1/`
/// - bash compl.  → `/usr/share/bash-completion/completions/<bin>`
/// - zsh  compl.  → `/usr/share/zsh/site-functions/_<bin>`
/// - fish compl.  → `/usr/share/fish/vendor_completions.d/<bin>.fish`
pub(crate) fn aur_extra_install_lines(
    crate_cfg: &anodizer_core::config::CrateConfig,
    binary_name: &str,
) -> Vec<String> {
    use anodizer_core::config::{ArchivesConfig, GenMode, completion_filename};

    let mut lines = Vec::new();

    // LICENSE — REQUIRED for non-common licenses; the archive auto-includes
    // `LICENSE*` at its root. Match any LICENSE* the tarball carries so the
    // line works regardless of extension (LICENSE, LICENSE.md, LICENSE-MIT).
    lines.push(
        "for _l in \"$srcdir\"/LICENSE*; do [ -e \"$_l\" ] && \
         install -Dm644 \"$_l\" \"$pkgdir/usr/share/licenses/$pkgname/$(basename \"$_l\")\"; done"
            .to_string(),
    );

    if let ArchivesConfig::Configs(cfgs) = &crate_cfg.archives {
        // man pages — install every file the archive's manpages dir carries.
        let mut seen_man = std::collections::HashSet::new();
        for cfg in cfgs {
            let Some(man) = cfg.manpages.as_ref() else {
                continue;
            };
            if matches!(man.mode(), GenMode::None) {
                continue;
            }
            let dst = man.resolved_dst();
            let dir = dst.strip_suffix('/').unwrap_or(dst);
            if seen_man.insert(dir.to_string()) {
                lines.push(format!(
                    "for _m in \"$srcdir/{dir}\"/*; do [ -e \"$_m\" ] && \
                     install -Dm644 \"$_m\" \"$pkgdir/usr/share/man/man1/$(basename \"$_m\")\"; done"
                ));
            }
        }

        // shell completions — bash/zsh/fish into their pacman vendor dirs.
        for cfg in cfgs {
            let Some(comp) = cfg.completions.as_ref() else {
                continue;
            };
            if matches!(comp.mode(), GenMode::None) {
                continue;
            }
            let dst = comp.resolved_dst();
            let dir = dst.strip_suffix('/').unwrap_or(dst);
            for (shell, dest_dir, dest_name) in [
                (
                    "bash",
                    "/usr/share/bash-completion/completions",
                    binary_name.to_string(),
                ),
                (
                    "zsh",
                    "/usr/share/zsh/site-functions",
                    format!("_{binary_name}"),
                ),
                (
                    "fish",
                    "/usr/share/fish/vendor_completions.d",
                    format!("{binary_name}.fish"),
                ),
            ] {
                if comp
                    .resolved_shells()
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(shell))
                {
                    let src_file = completion_filename(binary_name, shell);
                    lines.push(format!(
                        "[ -e \"$srcdir/{dir}/{src_file}\" ] && \
                         install -Dm644 \"$srcdir/{dir}/{src_file}\" \"$pkgdir{dest_dir}/{dest_name}\""
                    ));
                }
            }
        }
    }

    lines
}

/// Render a license string into the pacman `license=()` array entries.
///
/// A dual/multi-license SPDX expression (`MIT OR Apache-2.0`,
/// `MIT/Apache-2.0`) is split into its constituent SPDX ids
/// (`['MIT', 'Apache-2.0']`) — the convention modern AUR packages use. A
/// single id (or an expression the shared parser keeps literal, e.g. a `WITH`
/// exception or a parenthesised compound) renders as a one-element array. An
/// empty/blank license yields an empty array so the template emits
/// `license=()` (no spurious `license=('')`, which `namcap` lints).
pub(crate) fn aur_license_array(license: &str) -> Vec<String> {
    if license.trim().is_empty() {
        return Vec::new();
    }
    anodizer_core::license::parse_spdx_expression(license)
        .ids()
        .to_vec()
}

pub(crate) fn aur_resolve_fields(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<AurResolvedFields> {
    // AUR pkgver does not allow hyphens; replace with underscores.
    let version = ctx.version().replace('-', "_");

    // Default() resolution: name auto-suffix `-bin`, conflicts /
    // provides default to [base_name], pkgrel default `"1"`. The defaults
    // are split across two helpers (`aur_default_package_name` →
    // template-render → `aur_resolve_defaults`) to expose the default
    // rules to unit tests without standing up a full publish flow, while
    // ensuring `base_name` is derived from the rendered package name (so
    // `aur.name = "{{ .ProjectName }}-bin"` with an empty project_name
    // does not leak unrendered template syntax into conflicts/provides).
    let project_name_for_defaults = ctx.config.project_name.as_str();
    let raw_package_name = aur_default_package_name(aur_cfg, crate_name);
    // Render the resolved name through the template engine — users who set
    // `aur.name: "{{ .ProjectName }}-bin"` rely on this. On render failure
    // (typically a malformed template like `{{ unclosed`), surface a warning
    // and fall back to the raw value: a visible warning beats a silent
    // swallow without breaking a currently-malformed user build.
    let package_name = util::render_or_warn(ctx, log, "aur.name", &raw_package_name)?;
    let resolved_defaults = aur_resolve_defaults(aur_cfg, &package_name, project_name_for_defaults);

    // Fall back to project `metadata.*` when aur config unset.
    let description_raw = aur_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description = util::render_or_warn(ctx, log, "aur.description", description_raw)?;

    // PKGBUILD `license=()` is documented as RECOMMENDED but not required
    // per the Arch wiki (https://wiki.archlinux.org/title/PKGBUILD#license).
    // A dual-licensed crate (`MIT OR Apache-2.0`) must render as a multi-id
    // array `license=('MIT' 'Apache-2.0')`, mirroring real AUR -bin packages;
    // a single value would understate the licensing. The SPDX expression is
    // split via the shared parser into the modern-AUR SPDX-id convention.
    let license_raw = aur_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string))
        .unwrap_or_default();
    let license = aur_license_array(&license_raw);

    // PKGBUILD `url=` resolves through `homepage:` → crate metadata
    // homepage → the derived github release URL.
    let url_override = aur_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(crate_name))
        .map(|s| s.to_string());
    let url_raw = if let Some(u) = url_override {
        u
    } else if let Some(gh) = crate_cfg.release.as_ref().and_then(|r| r.github.as_ref()) {
        format!("https://github.com/{}/{}", gh.owner, gh.name)
    } else {
        anyhow::bail!(
            "aur: no url configured for '{}' and no release.github owner/name available. \
             Set `publish.aur.homepage` or configure `release.github` with owner and name.",
            crate_name
        );
    };
    // A user-supplied `aur.homepage` / `metadata.homepage` may carry template
    // syntax (`{{ .Tag }}`); render it so the PKGBUILD `url=` line ships the
    // resolved value, not the literal delimiters. The derived github URL has no
    // delimiters but rendering it is a no-op, keeping the path uniform.
    let url = util::render_or_warn(ctx, log, "aur.url", &url_raw)?;

    let maintainers = aur_cfg
        .maintainers
        .clone()
        .unwrap_or_else(|| ctx.config.meta_maintainers_for(crate_name).to_vec());
    // The Vec fields below default to empty when unset. The PKGBUILD_TEMPLATE
    // wraps each in a `{% if X | length > 0 %}...{% endif %}` guard so the
    // emitted PKGBUILD omits the corresponding `<key>=(...)` line entirely
    // when the list is empty — all of these arrays are optional per the
    // PKGBUILD spec (https://wiki.archlinux.org/title/PKGBUILD).
    let contributors = aur_cfg.contributors.clone().unwrap_or_default();
    let depends = aur_cfg.depends.clone().unwrap_or_default();
    let optdepends = aur_cfg.optdepends.clone().unwrap_or_default();
    // conflicts / provides come from the default resolver, which was
    // fed the *rendered* package name,
    // so `base_name` reflects post-template values when `project_name` is
    // empty.
    let conflicts = resolved_defaults.conflicts;
    let provides = resolved_defaults.provides;
    let replaces = aur_cfg.replaces.clone().unwrap_or_default();
    let backup = aur_cfg.backup.clone().unwrap_or_default();

    Ok(AurResolvedFields {
        package_name,
        version,
        pkgrel: resolved_defaults.pkgrel,
        description,
        license,
        url,
        maintainers,
        contributors,
        depends,
        optdepends,
        conflicts,
        provides,
        replaces,
        backup,
    })
}
