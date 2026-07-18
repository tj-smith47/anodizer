use anodizer_core::config::HomebrewConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};
use anyhow::Result;

/// Format preference for homebrew taps: `.tar.gz` (canonical) then `tgz`
/// (alias for the same wire format).
pub(crate) const HOMEBREW_PREFERRED_FORMATS: &[&str] = &["tar.gz", "tgz"];

/// Disambiguate a list of `(target, url, sha256, format)` tuples when the
/// same `(os, arch)` key appears more than once. Delegates to
/// [`crate::util::disambiguate_by_format`]; this wrapper exists to share the
/// caller-side tuple shape with the unit tests.
pub(crate) fn disambiguate_homebrew_archives(
    entries: Vec<(String, String, String, String)>,
    ids_was_set: bool,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<(String, String, String)>> {
    let deduped = crate::util::disambiguate_by_format(
        entries,
        |(target, _, _, _)| {
            let (os, arch) = anodizer_core::target::map_target(target);
            format!("{os}_{arch}")
        },
        |(_, _, _, fmt)| fmt.as_str(),
        |(_, url, _, _)| url.clone(),
        crate::util::DisambiguateConfig {
            preferred_formats: HOMEBREW_PREFERRED_FORMATS,
            ids_was_set,
            publisher_label: "homebrew",
            crate_name,
            logger: log,
        },
    )?;
    Ok(deduped
        .into_iter()
        .map(|(t, u, s, _fmt)| (t, u, s))
        .collect())
}

/// Resolved metadata strings for the formula: description, license,
/// homepage, and the rendered formula name. All fields are post-Tera
/// (rendered through `ctx.render_template`) and fall back to project
/// `metadata.*`.
pub(super) struct ResolvedMetadata {
    pub(super) description: String,
    pub(super) license: Option<String>,
    pub(super) homepage: Option<String>,
    pub(super) formula_name: String,
}

/// Resolve formula metadata strings with project-level `metadata.*` fallbacks
/// and Tera rendering applied.
pub(super) fn resolve_homebrew_metadata(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<ResolvedMetadata> {
    let description_raw = hb_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description = crate::util::render_or_warn(ctx, log, "brew.description", description_raw)?;
    let license = hb_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license_for(crate_name))
        .map(|l| crate::util::render_or_warn(ctx, log, "brew.license", l))
        .transpose()?;
    let homepage = hb_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(crate_name))
        .map(|h| crate::util::render_or_warn(ctx, log, "brew.homepage", h))
        .transpose()?;
    let formula_name_raw = hb_cfg.name.as_deref().unwrap_or(crate_name);
    let formula_name = crate::util::render_or_warn(ctx, log, "brew.name", formula_name_raw)?;
    Ok(ResolvedMetadata {
        description,
        license,
        homepage,
        formula_name,
    })
}

/// Pre-rendered Ruby code blocks emitted into the formula body.
pub(super) struct RenderedFormulaCode {
    pub(super) install: String,
    pub(super) test: String,
    pub(super) extra_install: Option<String>,
    pub(super) post_install: Option<String>,
}

/// Build the `install`, `test`, `extra_install`, and `post_install` blocks
/// from config + artifact metadata. Auto-generates multi-binary install
/// lines from ExtraBinaries metadata when no explicit install is set
/// the manifest.
pub(super) fn render_install_and_test_blocks(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<RenderedFormulaCode> {
    let is_strict = ctx.render_is_strict();
    let mut tmpl_vars = TemplateVars::new();
    tmpl_vars.set("name", crate_name);
    tmpl_vars.set("version", version);

    let install_raw = if let Some(ref custom_install) = hb_cfg.install {
        custom_install.clone()
    } else {
        let mut bin_names = std::collections::BTreeSet::new();
        for art in ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name)
        {
            for name in art.extra_binaries() {
                bin_names.insert(name);
            }
        }
        for art in ctx.artifacts.by_kind_and_crate(
            anodizer_core::artifact::ArtifactKind::UploadableBinary,
            crate_name,
        ) {
            if let Some(bin) = art.extra_binary() {
                if art.name() != bin {
                    // The fragment closes and reopens the Ruby string literal
                    // to emit `bin.install "<name>" => "<bin>"`; escape each
                    // side's contents individually so the structural quotes
                    // stay intact.
                    bin_names.insert(format!(
                        "{}\" => \"{}",
                        template::ruby_escape_str(art.name()),
                        template::ruby_escape_str(&bin)
                    ));
                } else {
                    bin_names.insert(bin);
                }
            }
        }
        if bin_names.is_empty() {
            format!("bin.install \"{}\"", template::ruby_escape_str(crate_name))
        } else {
            bin_names
                .into_iter()
                .map(|name| format!("bin.install \"{}\"", name))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    // Append completion + manpage installs (prebuilt files) and the
    // `generate_completions_from_executable` directive when configured. These
    // ride inside every per-OS `def install` block so a real Rust CLI formula
    // ships shell completions + manpages like ripgrep/fd/bat, not a bare
    // `bin.install`. Only appended to the auto-derived install block — when the
    // user hand-writes `install:`, they own the full block (including any
    // completions) and we must not double-emit.
    let install_raw = if hb_cfg.install.is_some() {
        install_raw
    } else {
        let mut extra = super::super::formula::build_completion_and_manpage_install_lines(
            hb_cfg.completions.as_ref(),
            hb_cfg.manpages.as_deref(),
        );
        if let Some(line) = hb_cfg
            .generate_completions_from_executable
            .as_ref()
            .and_then(super::super::cask::render_generate_completions)
        {
            extra.push(line);
        }
        if extra.is_empty() {
            install_raw
        } else {
            format!("{}\n{}", install_raw, extra.join("\n"))
        }
    };
    let install = crate::util::render_or_warn_with_vars(
        &tmpl_vars,
        log,
        "brew.install",
        &install_raw,
        is_strict,
    )?;
    let test_raw = hb_cfg.test.clone().unwrap_or_else(|| {
        format!(
            "system \"#{{bin}}/{}\", \"--version\"",
            template::ruby_escape_str(crate_name)
        )
    });
    let test =
        crate::util::render_or_warn_with_vars(&tmpl_vars, log, "brew.test", &test_raw, is_strict)?;

    let extra_install = hb_cfg
        .extra_install
        .as_deref()
        .map(|s| {
            crate::util::render_or_warn_with_vars(
                &tmpl_vars,
                log,
                "brew.extra_install",
                s,
                is_strict,
            )
        })
        .transpose()?;
    let post_install = hb_cfg
        .post_install
        .as_deref()
        .map(|s| {
            crate::util::render_or_warn_with_vars(
                &tmpl_vars,
                log,
                "brew.post_install",
                s,
                is_strict,
            )
        })
        .transpose()?;
    Ok(RenderedFormulaCode {
        install,
        test,
        extra_install,
        post_install,
    })
}

/// Filter `crate_name`'s `Archive` + `UploadableBinary` artifacts down to the
/// set a homebrew formula would draw from: keep only macOS/Linux targets (the
/// only OSes Homebrew installs on), drop universal-binary leftovers and raw
/// `gz` blobs, apply the `ids:` allow-list, and apply the `amd64_variant` /
/// `arm_variant` microarch selectors. Reads no url/sha256 metadata, so it only
/// answers "does a candidate exist" — a presence probe that is `bail!`-free,
/// distinct from [`collect_archive_entries`]'s render path (which errors when a
/// matched artifact is missing url/sha256).
///
/// The OS filter mirrors the nix aggregator's Linux/Darwin-only system mapping:
/// a windows, iOS/tvOS/watchOS, or any other non-macOS/Linux archive must never
/// reach a formula's url/sha256, or `brew install` on macOS/Linux would fetch an
/// un-installable artifact. macOS eligibility uses [`is_macos`] (genuine
/// `*-apple-darwin` only), NOT the broad [`is_darwin`]="apple" — the latter also
/// admits `aarch64-apple-ios`/`-tvos`/`-watchos`, which are buildable targets
/// but carry no `brew`-installable binary and would otherwise land in the
/// formula's untyped `# platform:` url block (a 404-class install). It also makes
/// [`crate_has_homebrew_archives`] report a non-eligible-only artifact set as
/// absence (`false`), so a determinism shard that produced only windows/iOS
/// archives self-skips instead of emitting a broken-url formula.
pub(super) fn homebrew_matching_artifacts<'a>(
    ctx: &'a Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
) -> Vec<&'a anodizer_core::artifact::Artifact> {
    let ids_filter = hb_cfg.ids.as_deref();
    let amd64_variant = hb_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    // Goarm defaults to "6" for Homebrew.
    let arm_variant = hb_cfg.arm_variant.as_deref().or(Some("6"));
    let mut all_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name);
    all_artifacts.extend(ctx.artifacts.by_kind_and_crate(
        anodizer_core::artifact::ArtifactKind::UploadableBinary,
        crate_name,
    ));
    all_artifacts
        .into_iter()
        // Homebrew installs only on macOS + Linux; a windows/iOS/other-OS
        // archive must never become a formula's url/sha256 (brew would fetch it
        // on macOS/Linux and 404-class fail). `is_macos` (genuine `*-apple-darwin`
        // only) excludes iOS/tvOS/watchOS, which the broad `is_darwin`="apple"
        // would wrongly admit. Mirrors nix's Linux/Darwin-only map. A target-less
        // artifact (empty triple) matches neither predicate and is excluded.
        .filter(|a| {
            let target = a.target.as_deref().unwrap_or("");
            anodizer_core::target::is_macos(target) || anodizer_core::target::is_linux(target)
        })
        // OnlyReplacingUnibins: exclude universal binaries that didn't replace
        // single-arch variants.
        .filter(|a| a.only_replacing_unibins())
        // Exclude raw `gz` archives (not `tar.gz`): Homebrew cannot
        // install a single-file compressed blob as an archive.
        .filter(|a| a.metadata.get("format").is_none_or(|f| f != "gz"))
        .filter(|a| {
            if let Some(ids) = ids_filter {
                a.metadata
                    .get("id")
                    .map(|id| ids.iter().any(|i| i == id))
                    .unwrap_or(false)
            } else {
                true
            }
        })
        // Filter by amd64_variant/arm_variant microarchitecture variant.
        .filter(|a| {
            let target = a.target.as_deref().unwrap_or("");
            let (_, arch) = anodizer_core::target::map_target(target);
            if arch == "amd64" {
                return a
                    .metadata
                    .get("amd64_variant")
                    .is_none_or(|v| v == amd64_variant);
            }
            if arch.starts_with("arm")
                && arch != "arm64"
                && let Some(want) = arm_variant
            {
                return a.metadata.get("arm_variant").is_none_or(|v| v == want);
            }
            true
        })
        .collect()
}

/// Collect, filter, and disambiguate archive entries (Archive +
/// UploadableBinary) for the formula. Returns `(target, url, sha256)`
/// tuples ready to feed into the formula renderer.
pub(super) fn collect_archive_entries(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<Vec<(String, String, String)>> {
    let ids_filter = hb_cfg.ids.as_deref();
    // Collect as (target, url, sha256, format) so the disambiguator can prefer
    // .tar.gz when multiple archives match the same OS/arch and ids: is unset.
    let raw_archive_data: Vec<(String, String, String, String)> =
        homebrew_matching_artifacts(ctx, hb_cfg, crate_name)
            .iter()
            .map(|a| {
                let target = a.target.as_deref().unwrap_or("");
                // When url_template is set, render it to produce the download URL;
                // otherwise use the artifact metadata URL (from the release stage).
                let url = if let Some(tmpl) = hb_cfg.url_template.as_deref() {
                    let (os, arch) = anodizer_core::target::map_target(target);
                    crate::util::render_url_template_with_ctx(
                        ctx,
                        tmpl,
                        a.name(),
                        version,
                        &arch,
                        &os,
                    )
                } else {
                    a.metadata
                        .get("url")
                        .map(|v| v.to_string())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "homebrew formula: artifact '{}' is missing 'url' metadata — \
                             ensure the release stage ran successfully and populated \
                             dist/artifacts.json",
                                a.name()
                            )
                        })?
                };
                let sha256 = a
                    .metadata
                    .get("sha256")
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "homebrew formula: artifact '{}' is missing sha256 metadata — \
                         ensure the checksum stage ran before the publish stage; \
                         without a valid sha256 the generated formula would fail \
                         `brew audit`",
                            a.name()
                        )
                    })?;
                // `format` feeds the multi-archive disambiguator (prefers .tar.gz
                // > tgz). Empty value just demotes this entry to lowest preference;
                // never reaches the rendered formula.
                let format = a.metadata.get("format").cloned().unwrap_or_default();
                Ok((target.to_string(), url, sha256, format))
            })
            .collect::<Result<Vec<_>>>()?;

    let archive_data =
        disambiguate_homebrew_archives(raw_archive_data, ids_filter.is_some(), crate_name, log)?;

    if archive_data.is_empty() {
        let ids_hint = ids_filter
            .map(|ids| format!("ids={ids:?}"))
            .unwrap_or_else(|| "ids=<none>".to_string());
        // Hint from the raw config, not the folded filter value, so a
        // defaulted selector reads `<default …>` while a configured one
        // prints plainly.
        let amd64_hint = hb_cfg.amd64_variant.map_or("<default v1>", |v| v.as_str());
        let arm_hint = hb_cfg.arm_variant.as_deref().unwrap_or("<default 6>");
        anyhow::bail!(
            "homebrew: no archives matched filters for '{crate_name}' — \
             formula would have empty url/sha256. Check your archive \
             configuration and homebrew filters ({ids_hint}, \
             amd64_variant={amd64_hint}, arm_variant={arm_hint}). At least one \
             Archive or UploadableBinary artifact must match."
        );
    }
    Ok(archive_data)
}
