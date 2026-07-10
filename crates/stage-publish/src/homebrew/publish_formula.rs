//! `publish_to_homebrew` — per-crate formula (and optional same-tap cask)
//! publisher.
use super::cask::generate_cask_from_context;
use super::commit_msg::render_commit_msg;
use super::formula::{FormulaOptions, generate_formula_with_opts};
use anodizer_core::config::HomebrewConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

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
struct ResolvedMetadata {
    description: String,
    license: Option<String>,
    homepage: Option<String>,
    formula_name: String,
}

/// Resolve formula metadata strings with project-level `metadata.*` fallbacks
/// and Tera rendering applied.
fn resolve_homebrew_metadata(
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
struct RenderedFormulaCode {
    install: String,
    test: String,
    extra_install: Option<String>,
    post_install: Option<String>,
}

/// Build the `install`, `test`, `extra_install`, and `post_install` blocks
/// from config + artifact metadata. Auto-generates multi-binary install
/// lines from ExtraBinaries metadata when no explicit install is set
/// the manifest.
fn render_install_and_test_blocks(
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
        let mut extra = super::formula::build_completion_and_manpage_install_lines(
            hb_cfg.completions.as_ref(),
            hb_cfg.manpages.as_deref(),
        );
        if let Some(line) = hb_cfg
            .generate_completions_from_executable
            .as_ref()
            .and_then(super::cask::render_generate_completions)
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
fn homebrew_matching_artifacts<'a>(
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
fn collect_archive_entries(
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

/// Owner/name/clone-path triple describing the tap checkout. Bundled to
/// keep helper signatures readable.
struct TapLocation<'a> {
    repo_owner: &'a str,
    repo_name: &'a str,
    repo_path: &'a Path,
}

/// Identity strings threaded through the commit/log/PR helpers: the crate
/// being published, the rendered formula name, and the version tag.
struct FormulaIdentity<'a> {
    crate_name: &'a str,
    formula_name: &'a str,
    version: &'a str,
}

/// Clone the tap repo into a tempdir and write the rendered formula.
/// Returns the on-disk formula path so the caller can stage it for the
/// subsequent commit.
fn clone_tap_and_write_formula(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    tap: &TapLocation<'_>,
    formula_name: &str,
    formula: &str,
    log: &StageLogger,
) -> Result<PathBuf> {
    let token = crate::util::resolve_repo_token(
        ctx,
        hb_cfg.repository.as_ref(),
        Some("HOMEBREW_TAP_TOKEN"),
    );
    crate::util::clone_repo(
        ctx,
        hb_cfg.repository.as_ref(),
        tap.repo_owner,
        tap.repo_name,
        token.as_deref(),
        tap.repo_path,
        "homebrew",
        log,
    )?;

    // Determine formula directory (the `directory` field).
    // Empty string means "tap repo root" — the `is_empty()` branch below
    // uses `repo_path` directly without joining, so the empty default is the
    // documented no-subdirectory mode (most Homebrew taps put formulae at
    // the root).
    let directory = hb_cfg.directory.clone().unwrap_or_default();
    let formula_dir = if directory.is_empty() {
        tap.repo_path.to_path_buf()
    } else {
        tap.repo_path.join(&directory)
    };
    std::fs::create_dir_all(&formula_dir)
        .with_context(|| format!("homebrew: create formula dir {}", formula_dir.display()))?;

    let formula_path = formula_dir.join(format!("{}.rb", formula_name));
    std::fs::write(&formula_path, formula)
        .with_context(|| format!("homebrew: write formula {}", formula_path.display()))?;

    log.status(&format!(
        "wrote Homebrew formula {}",
        formula_path.display()
    ));
    Ok(formula_path)
}

/// Side-result of optionally writing a cask file into the same tap clone.
#[derive(Default)]
struct CaskInTapOutcome {
    /// Cask name (for log/PR-body decoration) when a cask was written.
    cask_name: Option<String>,
    /// On-disk path of the written cask (for `git add`) when one was written.
    cask_path: Option<PathBuf>,
    /// Additional versioned alt-name `.rb` files (the
    /// `alternative_names:` versioned-file emission). Each entry is
    /// included in the commit set so the tap commit covers every file
    /// touched by this publish.
    versioned_paths: Vec<PathBuf>,
}

/// Render the same-tap cask that accompanies a formula, honoring the cask's
/// own `skip_upload`. Returns `Ok(None)` when no cask is configured or the
/// cask's `skip_upload` is truthy — the formula still publishes on its own.
///
/// Splits the cask's skip gate (evaluated here, once) from the pure
/// [`generate_cask_from_context`] render so the live publish path and the
/// offline schema validator share one render without double-warning.
pub(crate) fn render_same_tap_cask_for_crate(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<super::cask::CaskGenResult>> {
    let Some(cask_cfg) = hb_cfg.cask.as_ref() else {
        return Ok(None);
    };
    if crate::util::should_skip_upload(
        cask_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("homebrew cask for '{crate_name}'")),
    )? {
        return Ok(None);
    }
    let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg, log)?;
    Ok(Some(cask_result))
}

/// When a cask config is present alongside the formula config, generate and
/// write the cask into the same tap clone so the commit/push covers both
/// files in a single round-trip.
fn maybe_write_cask_into_tap(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    repo_path: &Path,
    log: &StageLogger,
) -> Result<CaskInTapOutcome> {
    let Some(cask_result) = render_same_tap_cask_for_crate(ctx, hb_cfg, crate_name, log)? else {
        return Ok(CaskInTapOutcome::default());
    };
    let cask_cfg = hb_cfg.cask.as_ref().ok_or_else(|| {
        anyhow::anyhow!("homebrew cask: cask config vanished for '{}'", crate_name)
    })?;

    // Honor `cask.directory:` so a tap can place
    // casks in a sub-tree. Defaults to "Casks". The cask config field
    // takes precedence; without it we land at the conventional
    // homebrew-cask path.
    let directory = super::resolve_cask_directory(cask_cfg.directory.as_deref(), ctx)?;
    let casks_dir = repo_path.join(&directory);
    std::fs::create_dir_all(&casks_dir).with_context(|| {
        format!(
            "homebrew cask: create {} dir {}",
            directory,
            casks_dir.display()
        )
    })?;

    let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
    std::fs::write(&cask_path, &cask_result.content)
        .with_context(|| format!("homebrew cask: write cask file {}", cask_path.display()))?;
    log.status(&format!("wrote Homebrew cask {}", cask_path.display()));

    // Versioned alt-name files. Each emits a sibling `.rb` so users can
    // `brew install <pkg>@<version>` for a pinned/downgrade install path.
    let mut versioned_paths: Vec<PathBuf> = Vec::with_capacity(cask_result.versioned_files.len());
    for (alt_name, body) in &cask_result.versioned_files {
        let alt_path = casks_dir.join(format!("{}.rb", alt_name));
        std::fs::write(&alt_path, body).with_context(|| {
            format!(
                "homebrew cask: write versioned cask file {}",
                alt_path.display()
            )
        })?;
        log.status(&format!("wrote Homebrew cask {}", alt_path.display()));
        versioned_paths.push(alt_path);
    }

    Ok(CaskInTapOutcome {
        cask_name: Some(cask_result.cask_name),
        cask_path: Some(cask_path),
        versioned_paths,
    })
}

/// Stage the formula (and optional cask), render the commit message, and
/// run the commit/push round-trip. Logs the per-outcome status line. The
/// `branch` argument is the pre-resolved push target (None ⇒ default).
#[allow(clippy::too_many_arguments)]
fn commit_files_to_tap(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    ident: &FormulaIdentity<'_>,
    tap: &TapLocation<'_>,
    formula_path: &Path,
    cask: &CaskInTapOutcome,
    branch: Option<&str>,
    log: &StageLogger,
) -> Result<crate::util::CommitOutcome> {
    let formula_lossy = formula_path.to_string_lossy();
    let cask_lossy = cask.cask_path.as_ref().map(|p| p.to_string_lossy());
    let versioned_lossy: Vec<std::borrow::Cow<'_, str>> = cask
        .versioned_paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect();
    let mut files_to_commit: Vec<&str> = vec![&formula_lossy];
    if let Some(ref cl) = cask_lossy {
        files_to_commit.push(cl);
    }
    for v in &versioned_lossy {
        files_to_commit.push(v.as_ref());
    }

    let kind = if cask.cask_name.is_some() {
        "formula and cask"
    } else {
        "formula"
    };
    let commit_msg = render_commit_msg(
        hb_cfg.commit_msg_template.as_deref(),
        ident.formula_name,
        ident.version,
        kind,
        log,
        ctx.render_is_strict(),
    )?;

    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref(), log)?;
    let outcome = crate::util::commit_and_push_with_opts(
        tap.repo_path,
        &files_to_commit,
        &commit_msg,
        branch,
        "homebrew",
        &commit_opts,
        log,
    )?;
    match outcome {
        crate::util::CommitOutcome::Pushed => {
            if let Some(ref cask_name) = cask.cask_name {
                log.status(&format!(
                    "Homebrew tap {}/{} updated with formula '{}' and cask '{}'",
                    tap.repo_owner, tap.repo_name, ident.formula_name, cask_name
                ));
            } else {
                log.status(&format!(
                    "Homebrew tap {}/{} updated for '{}'",
                    tap.repo_owner, tap.repo_name, ident.crate_name
                ));
            }
        }
        crate::util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, homebrew formula for '{}' already up to date",
                ident.formula_name
            ));
        }
    }
    Ok(outcome)
}

/// Submit (or record) the optional PR for the tap update. The PR title
/// and body switch between formula-only and formula+cask phrasings to
/// match the kind of file(s) that were committed.
fn submit_homebrew_pr(
    ctx: &mut Context,
    repo_for_pr: Option<anodizer_core::config::RepositoryConfig>,
    ident: &FormulaIdentity<'_>,
    tap: &TapLocation<'_>,
    cask_name: Option<&str>,
    pr_branch: &str,
    log: &StageLogger,
) {
    let formula_name = ident.formula_name;
    let version = ident.version;
    let (pr_title, pr_body) = if let Some(cask_name) = cask_name {
        (
            format!(
                "Update {} formula and {} cask to {}",
                formula_name, cask_name, version
            ),
            format!(
                "## Formula\n- **Name**: {}\n- **Version**: {}\n\n## Cask\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
                formula_name, version, cask_name, version
            ),
        )
    } else {
        (
            format!("Update {} formula to {}", formula_name, version),
            format!(
                "## Formula\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
                formula_name, version
            ),
        )
    };

    let pr_outcome = crate::util::maybe_submit_pr(
        tap.repo_path,
        repo_for_pr.as_ref(),
        &crate::util::PrOrigin {
            repo_owner: tap.repo_owner,
            repo_name: tap.repo_name,
            branch_name: pr_branch,
            // Homebrew formula publishes commit directly to the tap
            // branch; the optional PR is informational. The cask/winget/krew
            // `update_existing_pr:` flag has no analogue on `HomebrewConfig`
            // because there's no real "blocked queue" to recover from here.
            update_existing_pr: false,
        },
        &pr_title,
        &pr_body,
        "homebrew",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );

    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }
}

/// A rendered formula plus the formula name used as its `.rb` filename stem.
pub(crate) struct RenderedFormula {
    /// The rendered Ruby formula body.
    pub(crate) formula: String,
    /// The post-Tera formula name (filename stem + `class` token source).
    pub(crate) formula_name: String,
}

/// Render the Ruby formula a live publish would write for `crate_name`,
/// honoring `skip_upload` and the `if:` condition.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip_upload`
/// truthy or a falsy `if`) — nothing to render or validate. The live publish
/// path and the offline schema validator both produce the formula through the
/// same skip-unaware [`render_formula_inner`] so the validated document is
/// byte-for-byte what a release pushes.
///
/// Errors when the crate carries no `homebrew` block or no archive artifact
/// matches the configured filters (a release always builds at least one). A
/// sharded snapshot that built no matching archive surfaces as that error; the
/// validator treats it as a skip via [`crate_has_homebrew_archives`].
pub(crate) fn render_homebrew_formula_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<RenderedFormula>> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;
    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no homebrew config for '{}'", crate_name))?;

    if crate::util::should_skip_upload(
        hb_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("homebrew for '{crate_name}'")),
    )? {
        return Ok(None);
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        hb_cfg.if_condition.as_deref(),
        &format!("homebrew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped homebrew for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(None);
    }

    let github_slug = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));
    let rendered = render_formula_inner(ctx, hb_cfg, crate_name, github_slug, log)?;
    Ok(Some(rendered))
}

/// True when at least one macOS/Linux archive artifact (`Archive` or
/// `UploadableBinary`) for `crate_name` survives the homebrew filters — i.e.
/// the formula render has a candidate to point at. A sharded snapshot that
/// built no homebrew-eligible archive (e.g. a windows-only determinism shard)
/// returns false so the validator can SKIP rather than trip the publisher's
/// "no archives matched" guard.
///
/// The macOS/Linux OS filter lives in [`homebrew_matching_artifacts`]: a
/// windows-only artifact set reports as `false` (absence) exactly as nix's
/// `crate_has_nix_archive` reports `Ok(false)` for a windows-only shard.
///
/// This is presence-only: it does NOT read url/sha256, so it returns `true`
/// even for a matched (macOS/Linux) artifact whose metadata is incomplete.
/// That is deliberate — a present-but-broken artifact is a real defect the
/// caller must surface by then calling the render (which `Err`s), not silently
/// skip. The OS filter does not swallow that: a broken macOS/Linux artifact is
/// still eligible, so the probe returns `true` and the render surfaces it.
pub(crate) fn crate_has_homebrew_archives(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
) -> bool {
    !homebrew_matching_artifacts(ctx, hb_cfg, crate_name).is_empty()
}

/// Skip-unaware formula render: resolve metadata, build the install/test
/// blocks, collect + disambiguate archive entries, and produce the Ruby body.
/// The skip / `if` gate is evaluated by the callers — both the live publish
/// path (which has already evaluated it) and
/// [`render_homebrew_formula_for_crate`] — so each resolved-with-warning value
/// is logged exactly once.
fn render_formula_inner(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    github_slug: Option<String>,
    log: &StageLogger,
) -> Result<RenderedFormula> {
    let version = ctx.version();
    let meta = resolve_homebrew_metadata(ctx, hb_cfg, crate_name, log)?;
    let code = render_install_and_test_blocks(ctx, hb_cfg, crate_name, &version, log)?;

    // User-supplied free-text stanzas are template-rendered here — the only
    // scope with the real `Context`+`log` — so a value like
    // `caveats: "see {{ .Tag }}"` resolves before reaching the generator (which
    // holds only a bare `tera::Context`). Mirrors `resolve_homebrew_metadata`'s
    // handling of description/homepage/license. Per-crate Tag/Version scoping is
    // inherited via the same `ctx`.
    let caveats = hb_cfg
        .caveats
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "brew.caveats", s))
        .transpose()?;
    let custom_require = hb_cfg
        .custom_require
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "brew.custom_require", s))
        .transpose()?;
    let custom_block = hb_cfg
        .custom_block
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "brew.custom_block", s))
        .transpose()?;
    let plist = hb_cfg
        .plist
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "brew.plist", s))
        .transpose()?;
    let service = hb_cfg
        .service
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "brew.service", s))
        .transpose()?;

    let opts = FormulaOptions {
        homepage: meta.homepage.as_deref(),
        github_slug,
        dependencies: hb_cfg.dependencies.as_deref(),
        conflicts: hb_cfg.conflicts.as_deref(),
        caveats: caveats.as_deref(),
        extra_install: code.extra_install.as_deref(),
        post_install: code.post_install.as_deref(),
        download_strategy: hb_cfg.download_strategy.as_deref(),
        url_headers: hb_cfg.url_headers.as_deref(),
        custom_require: custom_require.as_deref(),
        custom_block: custom_block.as_deref(),
        plist: plist.as_deref(),
        service: service.as_deref(),
        livecheck: super::formula::render_livecheck(hb_cfg.livecheck.as_ref(), log),
        // Render the `license` stanza from the parsed SPDX expression so a dual
        // license (`Apache-2.0 OR MIT`) becomes `license any_of: [...]` rather
        // than an invalid bare string. `None` when no license resolved → the
        // template omits the stanza.
        license_stanza: meta
            .license
            .as_deref()
            .and_then(super::formula::render_formula_license),
    };

    let archive_data = collect_archive_entries(ctx, hb_cfg, crate_name, &version, log)?;
    let archives: Vec<(&str, &str, &str)> = archive_data
        .iter()
        .map(|(t, u, s)| (t.as_str(), u.as_str(), s.as_str()))
        .collect();

    let formula_name = meta.formula_name.as_str();
    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: formula_name,
            version: &version,
            description: &meta.description,
            // FORMULA_TEMPLATE wraps `license` in `{% if license %}`, so empty
            // string renders as no `license` stanza. Homebrew formulae accept
            // omitting the license line (lint warns but does not error); the
            // formula remains installable.
            license: meta.license.as_deref().unwrap_or(""),
        },
        &archives,
        &super::formula::FormulaCode {
            install: &code.install,
            test: &code.test,
        },
        &opts,
    )?;

    // Final-text chokepoint shared by the live publish path and the offline
    // prepublish guard (both reach the formula string only through here): a
    // residual `{{ … }}` means a config field escaped rendering — fail strict,
    // warn lenient, before the formula is written or pushed. Ruby `#{}`
    // interpolation is not scanned, so completion/version interpolation is safe.
    crate::util::guard_no_unrendered(ctx, log, "homebrew formula", &formula)?;

    Ok(RenderedFormula {
        formula,
        formula_name: meta.formula_name,
    })
}

/// Render and push a Homebrew formula/cask for `crate_name`.
///
/// Returns `Ok(true)` when an actual git push was made to the tap repo;
/// `Ok(false)` when the publish was skipped (skip_upload, dry-run, or
/// any future early-exit guard). The caller (Publisher::run) uses the
/// boolean to decide whether to record rollback evidence — if no push
/// happened there's nothing to revert, and recording phantom evidence
/// would cause the rollback orchestrator to attempt a git revert HEAD
/// in a temp clone that has nothing this run actually changed.
pub fn publish_to_homebrew(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;

    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no homebrew config for '{}'", crate_name))?;

    if crate::util::should_skip_upload(
        hb_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("homebrew for '{crate_name}'")),
    )? {
        return Ok(false);
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        hb_cfg.if_condition.as_deref(),
        &format!("homebrew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped homebrew for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(false);
    }

    let (repo_owner, repo_name) = crate::util::resolve_repo_owner_name(hb_cfg.repository.as_ref())
        .ok_or_else(|| anyhow::anyhow!("homebrew: no repository config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Homebrew tap {}/{} for '{}'",
            repo_owner, repo_name, crate_name
        ));
        return Ok(false);
    }

    let version = ctx.version();

    // Clone the borrowed config slices upfront so the later `&mut ctx` calls
    // (record_publisher_outcome, maybe_submit_pr) don't conflict with the
    // immutable borrow held by `hb_cfg` / `publish`.
    let hb_cfg_owned: HomebrewConfig = hb_cfg.clone();
    let github_slug = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));

    // The skip / `if` / dry-run gates above already ran, so render via the
    // skip-unaware inner — re-running the gate here would double every
    // resolved-with-warning value's log line.
    let rendered = render_formula_inner(ctx, &hb_cfg_owned, crate_name, github_slug, log)?;
    let formula = rendered.formula;
    let formula_name = rendered.formula_name.as_str();

    let tmp_dir = tempfile::tempdir().context("homebrew: create temp dir")?;
    let tap = TapLocation {
        repo_owner: &repo_owner,
        repo_name: &repo_name,
        repo_path: tmp_dir.path(),
    };
    let ident = FormulaIdentity {
        crate_name,
        formula_name,
        version: &version,
    };

    let formula_path =
        clone_tap_and_write_formula(ctx, &hb_cfg_owned, &tap, formula_name, &formula, log)?;

    let cask = maybe_write_cask_into_tap(ctx, &hb_cfg_owned, crate_name, tap.repo_path, log)?;

    let branch = crate::util::resolve_branch(ctx, hb_cfg_owned.repository.as_ref());

    let outcome = commit_files_to_tap(
        ctx,
        &hb_cfg_owned,
        &ident,
        &tap,
        &formula_path,
        &cask,
        branch.as_deref(),
        log,
    )?;

    let pr_branch = branch.as_deref().unwrap_or("main");
    submit_homebrew_pr(
        ctx,
        hb_cfg_owned.repository.clone(),
        &ident,
        &tap,
        cask.cask_name.as_deref(),
        pr_branch,
        log,
    );

    Ok(outcome.is_pushed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::CommitOutcome;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, CrateConfig, GitRepoConfig, HomebrewCaskCompletions, HomebrewCaskConfig,
        HomebrewCaskGeneratedCompletions, HomebrewCaskURL, HomebrewConfig, HomebrewDependency,
        HomebrewLivecheck, PublishConfig, PullRequestConfig, ReleaseConfig, RepositoryConfig,
        StringOrBool, WorkspaceConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::fake_tool::{FakeToolDir, PathGuard};
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn commit_outcome_is_pushed() {
        assert!(CommitOutcome::Pushed.is_pushed());
        assert!(!CommitOutcome::NoChanges.is_pushed());
    }

    fn quiet_log() -> StageLogger {
        StageLogger::new("homebrew-test", Verbosity::Quiet)
    }

    /// Install a `gh` stub that exits non-zero on `--version` so the PR
    /// transport's `gh_is_available()` probe reports false, then prepend it
    /// to `PATH`. This makes the PR submission path deterministic (it routes
    /// to the gh-absent / token-driven fallback instead of a LIVE
    /// `gh pr create` against github.com on a host that has a real,
    /// authenticated `gh` in PATH). Returns the `FakeToolDir` holder (keeps
    /// the stub on disk) plus the `PathGuard` (restores `PATH` + releases the
    /// env mutex on drop) — both must be held for the test's duration. Tests
    /// using this MUST be `#[serial(path_env)]` because the guard mutates
    /// process `PATH`. Mirrors `util/pr.rs::gh_absent_path`.
    fn gh_absent() -> (FakeToolDir, PathGuard) {
        let tools = FakeToolDir::new();
        tools.tool("gh").exit(1).install();
        let guard = tools.activate();
        (tools, guard)
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        anodizer_core::test_helpers::git_test_ok(dir, args)
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        anodizer_core::test_helpers::git_test_stdout(dir, args)
    }

    /// Build a bare tap repo seeded with one commit on `branch`. Returns the
    /// bare repo path (a usable local `git clone` URL) plus the holder
    /// tempdir. The publisher clones this via the `git.url` SSH branch
    /// (which is a plain `git clone <localpath>` for a filesystem path),
    /// commits the formula, and pushes back to it. The seeded bare repo is
    /// the assertion surface: we inspect its landed `.rb` content + the
    /// commit subject after the publish.
    fn make_bare_tap(branch: &str) -> (String, tempfile::TempDir) {
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");

        git_ok(bare.path(), &["init", "--bare", "-b", branch]);
        git_ok(seed.path(), &["init", "-b", branch]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "T"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "tap\n").unwrap();
        git_ok(seed.path(), &["add", "README"]);
        git_ok(seed.path(), &["commit", "-m", "seed tap"]);
        // `git remote add` takes a path; pass it as an OsStr arg.
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git remote add origin failed"
        );
        git_ok(seed.path(), &["push", "-u", "origin", branch]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// Read the rendered formula `.rb` that landed on the bare tap's
    /// `branch` ref (formula lives at the tap root unless `directory:` is
    /// set). Uses `git show <branch>:<path>` so we read the pushed object,
    /// not a stale working tree.
    fn tap_show(bare: &Path, branch: &str, path: &str) -> String {
        git_stdout(bare, &["show", &format!("{branch}:{path}")])
    }

    /// Archive artifact carrying url + sha256 + format metadata for `mytool`.
    fn archive(target: &str, url: &str, sha: &str) -> Artifact {
        let mut metadata = HashMap::new();
        metadata.insert("url".to_string(), url.to_string());
        metadata.insert("sha256".to_string(), sha.to_string());
        metadata.insert("format".to_string(), "tar.gz".to_string());
        Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/tmp/{target}.tar.gz")),
            name: format!("mytool-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: "mytool".to_string(),
            metadata,
            size: None,
        }
    }

    /// A `HomebrewConfig` whose `git.url` points the clone at a local bare
    /// tap, with `owner`/`name`/`branch` set so owner-name resolution and the
    /// push target match the seeded ref.
    fn hb_cfg_local(bare_url: &str, branch: &str) -> HomebrewConfig {
        HomebrewConfig {
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("homebrew-tap".to_string()),
                branch: Some(branch.to_string()),
                git: Some(GitRepoConfig {
                    url: Some(bare_url.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Build a single-crate (top-level) Context wired to publish `mytool` to
    /// the homebrew tap with the supplied artifacts. Version resolves to
    /// `1.2.3` (tag `v1.2.3` via the builder default).
    fn single_crate_ctx(hb: HomebrewConfig, artifacts: Vec<Artifact>) -> Context {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(anodizer_core::config::ScmRepoConfig {
                        owner: "myorg".to_string(),
                        name: "mytool".to_string(),
                    }),
                    ..Default::default()
                }),
                publish: Some(PublishConfig {
                    homebrew: Some(hb),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        for a in artifacts {
            ctx.artifacts.add(a);
        }
        ctx
    }

    // ===================================================================
    // collect_archive_entries / homebrew_matching_artifacts — filter +
    // disambiguation + error paths feeding the formula renderer.
    // ===================================================================

    /// `collect_archive_entries` returns one `(target,url,sha256)` tuple per
    /// matching archive, carrying the artifact's url + sha256 verbatim.
    #[test]
    fn collect_archive_entries_returns_url_sha_per_archive() {
        let hb = HomebrewConfig::default();
        let ctx = single_crate_ctx(
            hb.clone(),
            vec![
                archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
                archive(
                    "x86_64-unknown-linux-gnu",
                    "https://e/linux.tar.gz",
                    "shalin",
                ),
            ],
        );
        let got =
            collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log()).expect("collect");
        assert_eq!(got.len(), 2);
        let arm = got
            .iter()
            .find(|(t, _, _)| t == "aarch64-apple-darwin")
            .expect("arm entry");
        assert_eq!(arm.1, "https://e/arm.tar.gz");
        assert_eq!(arm.2, "shaarm");
    }

    /// A matched artifact missing `sha256` is a real defect: the formula
    /// would fail `brew audit`. `collect_archive_entries` must `Err` naming
    /// the artifact + the checksum-stage remediation, not emit an empty sha.
    #[test]
    fn collect_archive_entries_errors_on_missing_sha256() {
        let hb = HomebrewConfig::default();
        let mut art = archive("x86_64-unknown-linux-gnu", "https://e/linux.tar.gz", "x");
        art.metadata.remove("sha256");
        let ctx = single_crate_ctx(hb.clone(), vec![art]);
        let err = collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log())
            .expect_err("missing sha256 must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256"), "{msg}");
        assert!(msg.contains("checksum stage"), "{msg}");
    }

    /// `url_template` overrides the artifact's url metadata: the rendered URL
    /// is computed from the template (os/arch/version) rather than copied
    /// from `metadata.url`. Proves the template branch of the url resolver.
    #[test]
    fn collect_archive_entries_renders_url_template() {
        let hb = HomebrewConfig {
            url_template: Some(
                "https://dl/{{ .Version }}/{{ .Os }}-{{ .Arch }}.tar.gz".to_string(),
            ),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb.clone(),
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://ignored/original.tar.gz",
                "shalin",
            )],
        );
        let got =
            collect_archive_entries(&ctx, &hb, "mytool", "1.2.3", &quiet_log()).expect("collect");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].1, "https://dl/1.2.3/linux-amd64.tar.gz",
            "url_template must drive the download URL, not metadata.url"
        );
    }

    /// The `ids:` allow-list filters the archive set: an artifact whose `id`
    /// is not listed is dropped from the formula's candidate set.
    #[test]
    fn homebrew_matching_artifacts_honors_ids_allowlist() {
        let hb = HomebrewConfig {
            ids: Some(vec!["keepme".to_string()]),
            ..Default::default()
        };
        let mut keep = archive("aarch64-apple-darwin", "https://e/keep.tar.gz", "k");
        keep.metadata.insert("id".to_string(), "keepme".to_string());
        let mut drop = archive("x86_64-unknown-linux-gnu", "https://e/drop.tar.gz", "d");
        drop.metadata.insert("id".to_string(), "other".to_string());
        let ctx = single_crate_ctx(hb.clone(), vec![keep, drop]);
        let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
        assert_eq!(matched.len(), 1, "only the allow-listed id survives");
        assert_eq!(
            matched[0].metadata.get("id").map(|s| s.as_str()),
            Some("keepme")
        );
    }

    /// A typed `amd64_variant: v3` selector matches the v3-tagged amd64
    /// archive and drops the explicitly-v1-tagged one — the positive half of
    /// the enum conversion (a typo'd level now dies at config parse; a valid
    /// level keeps selecting exactly the tuned archive).
    #[test]
    fn homebrew_matching_artifacts_selects_declared_amd64_variant() {
        let hb = HomebrewConfig {
            amd64_variant: Some(anodizer_core::config::Amd64Variant::V3),
            ..Default::default()
        };
        let mut v3 = archive("x86_64-unknown-linux-gnu", "https://e/v3.tar.gz", "s3");
        v3.metadata
            .insert("amd64_variant".to_string(), "v3".to_string());
        let mut v1 = archive("x86_64-unknown-linux-gnu", "https://e/v1.tar.gz", "s1");
        v1.metadata
            .insert("amd64_variant".to_string(), "v1".to_string());
        let ctx = single_crate_ctx(hb.clone(), vec![v3, v1]);
        let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
        assert_eq!(
            matched.len(),
            1,
            "only the v3-tagged archive matches a v3 selector"
        );
        assert_eq!(
            matched[0].metadata.get("url").map(String::as_str),
            Some("https://e/v3.tar.gz")
        );
    }

    /// A raw single-file `gz` blob (not `tar.gz`) cannot be installed as a
    /// Homebrew archive; the presence probe excludes it.
    #[test]
    fn homebrew_matching_artifacts_excludes_raw_gz() {
        let hb = HomebrewConfig::default();
        let mut gz = archive("x86_64-unknown-linux-gnu", "https://e/blob.gz", "g");
        gz.metadata.insert("format".to_string(), "gz".to_string());
        let ctx = single_crate_ctx(hb.clone(), vec![gz]);
        assert!(
            homebrew_matching_artifacts(&ctx, &hb, "mytool").is_empty(),
            "a raw .gz blob must not count as a homebrew archive candidate"
        );
        assert!(
            !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
            "crate_has_homebrew_archives must agree with the presence probe"
        );
    }

    /// Homebrew installs on macOS + Linux only: a windows archive is NOT an
    /// eligible candidate, so the presence probe (and thus
    /// `crate_has_homebrew_archives`) excludes it. Guards the failure-hiding
    /// class where a windows `.zip` would otherwise render a flat windows-url
    /// formula that 404s `brew install` on macOS/Linux.
    #[test]
    fn homebrew_matching_artifacts_excludes_windows() {
        let hb = HomebrewConfig::default();
        let win = archive("x86_64-pc-windows-msvc", "https://e/win.zip", "w");
        let mac = archive("aarch64-apple-darwin", "https://e/mac.tar.gz", "m");
        let ctx = single_crate_ctx(hb.clone(), vec![win, mac]);
        let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
        assert_eq!(matched.len(), 1, "only the macOS archive is eligible");
        assert_eq!(
            matched[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the windows archive must be filtered out"
        );
    }

    /// The apple-non-macOS targets (`*-apple-ios`/`-tvos`/`-watchos`) are
    /// buildable but carry no `brew`-installable binary; the broad `is_darwin`
    /// ("apple") predicate would wrongly admit them (they land in the formula's
    /// untyped `# platform:` url block — a 404-class install). The macOS-specific
    /// `is_macos` eligibility must exclude them while keeping genuine macOS.
    #[test]
    fn homebrew_matching_artifacts_excludes_apple_non_macos() {
        let hb = HomebrewConfig::default();
        let ios = archive("aarch64-apple-ios", "https://e/ios.tar.gz", "i");
        let tvos = archive("aarch64-apple-tvos", "https://e/tvos.tar.gz", "t");
        let watchos = archive("aarch64-apple-watchos", "https://e/watchos.tar.gz", "w");
        let mac = archive("aarch64-apple-darwin", "https://e/mac.tar.gz", "m");
        let ctx = single_crate_ctx(hb.clone(), vec![ios, tvos, watchos, mac]);
        let matched = homebrew_matching_artifacts(&ctx, &hb, "mytool");
        assert_eq!(
            matched.len(),
            1,
            "only the genuine macOS archive is eligible; ios/tvos/watchos excluded"
        );
        assert_eq!(
            matched[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the apple-non-macOS archives must be filtered out"
        );
    }

    /// A target-less archive (no triple) matches neither `is_macos` nor
    /// `is_linux`, so the OS filter excludes it — the presence probe reports
    /// absence rather than routing it through a flat-url formula. Documents the
    /// intended behavior of the `unwrap_or("")` fallback in the filter.
    #[test]
    fn homebrew_matching_artifacts_excludes_target_less() {
        let hb = HomebrewConfig::default();
        let mut targetless = archive("x86_64-unknown-linux-gnu", "https://e/x.tar.gz", "s");
        targetless.target = None;
        let ctx = single_crate_ctx(hb.clone(), vec![targetless]);
        assert!(
            homebrew_matching_artifacts(&ctx, &hb, "mytool").is_empty(),
            "a target-less archive is not a homebrew candidate"
        );
        assert!(
            !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
            "crate_has_homebrew_archives must agree the target-less set is absent"
        );
    }

    /// A windows-ONLY artifact set carries no homebrew-eligible archive, so the
    /// presence probe reports absence — mirroring nix's `Ok(false)` for a
    /// windows-only shard, which lets the emission validator self-skip.
    #[test]
    fn crate_has_homebrew_archives_false_for_windows_only() {
        let hb = HomebrewConfig::default();
        let win = archive("x86_64-pc-windows-msvc", "https://e/win.zip", "w");
        let ctx = single_crate_ctx(hb.clone(), vec![win]);
        assert!(
            !crate_has_homebrew_archives(&ctx, &hb, "mytool"),
            "a windows-only set is not homebrew-eligible"
        );
    }

    /// `crate_has_homebrew_archives` is presence-only: a matched artifact with
    /// NO url/sha256 still returns true (the caller surfaces the broken
    /// metadata via the render `Err`, not a silent skip).
    #[test]
    fn crate_has_homebrew_archives_true_even_when_metadata_incomplete() {
        let hb = HomebrewConfig::default();
        let mut art = archive("x86_64-unknown-linux-gnu", "https://e/x.tar.gz", "s");
        art.metadata.remove("url");
        art.metadata.remove("sha256");
        let ctx = single_crate_ctx(hb.clone(), vec![art]);
        assert!(
            crate_has_homebrew_archives(&ctx, &hb, "mytool"),
            "presence probe must report present-but-broken artifacts as present"
        );
    }

    // ===================================================================
    // render_homebrew_formula_for_crate / render_formula_inner — the Ruby
    // body the publisher would write.
    // ===================================================================

    /// The rendered formula carries the PascalCase class name, the version,
    /// each archive url + sha256, and a dependency declaration. Pins the
    /// load-bearing formula content the tap commit would carry.
    #[test]
    fn render_formula_for_crate_emits_class_url_sha_and_deps() {
        let hb = HomebrewConfig {
            dependencies: Some(vec![HomebrewDependency {
                name: "openssl".to_string(),
                os: None,
                dep_type: None,
                version: None,
            }]),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![
                archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
                archive(
                    "x86_64-unknown-linux-gnu",
                    "https://e/linux.tar.gz",
                    "shalin",
                ),
            ],
        );
        let rendered = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render ok")
            .expect("not skipped");
        let body = &rendered.formula;
        assert_eq!(rendered.formula_name, "mytool");
        assert!(
            body.contains("class Mytool < Formula"),
            "class line:\n{body}"
        );
        assert!(body.contains("version \"1.2.3\""), "version:\n{body}");
        assert!(body.contains("https://e/arm.tar.gz"), "arm url:\n{body}");
        assert!(body.contains("shaarm"), "arm sha:\n{body}");
        assert!(
            body.contains("https://e/linux.tar.gz"),
            "linux url:\n{body}"
        );
        assert!(body.contains("depends_on \"openssl\""), "dep:\n{body}");
    }

    /// `name:` override changes both the rendered class token and the
    /// `formula_name` (the `.rb` filename stem the publisher writes).
    #[test]
    fn render_formula_for_crate_honors_name_override() {
        let hb = HomebrewConfig {
            name: Some("rebranded".to_string()),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let rendered = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped");
        assert_eq!(rendered.formula_name, "rebranded");
        assert!(
            rendered.formula.contains("class Rebranded < Formula"),
            "{}",
            rendered.formula
        );
    }

    /// `skip_upload: true` makes the render-for-validation entry return
    /// `Ok(None)` (nothing to render) — distinct from an error.
    #[test]
    fn render_formula_for_crate_skip_upload_returns_none() {
        let hb = HomebrewConfig {
            skip_upload: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let got = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log()).expect("ok");
        assert!(got.is_none(), "skip_upload=true must render None");
    }

    /// A falsy `if:` condition skips the render (returns `Ok(None)`).
    #[test]
    fn render_formula_for_crate_falsy_if_returns_none() {
        let hb = HomebrewConfig {
            if_condition: Some("false".to_string()),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let got = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log()).expect("ok");
        assert!(got.is_none(), "falsy `if` must render None");
    }

    // ===================================================================
    // publish_to_homebrew — full clone → write → commit → push round-trip
    // against a local bare tap (direct-push path; PR disabled by default).
    // ===================================================================

    /// Happy path, single-crate mode: the publisher clones the local bare
    /// tap, writes `mytool.rb`, commits, and pushes. Asserts (1) the return
    /// is `Ok(true)` (a real push happened), (2) the formula `.rb` landed on
    /// the tap's branch ref with the correct class + version + url + sha, and
    /// (3) the commit subject names the formula + version.
    #[test]
    fn publish_to_homebrew_direct_push_lands_formula_single_crate() {
        let (bare_url, bare) = make_bare_tap("main");
        let hb = hb_cfg_local(&bare_url, "main");
        let mut ctx = single_crate_ctx(
            hb,
            vec![
                archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
                archive(
                    "x86_64-unknown-linux-gnu",
                    "https://e/linux.tar.gz",
                    "shalin",
                ),
            ],
        );
        let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed, "a real push must return Ok(true)");

        let bare_path = Path::new(&bare_url);
        let formula = tap_show(bare_path, "main", "mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        assert!(formula.contains("version \"1.2.3\""), "{formula}");
        assert!(formula.contains("https://e/arm.tar.gz"), "{formula}");
        assert!(formula.contains("shalin"), "{formula}");

        let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
        assert!(
            subject.contains("mytool") && subject.contains("1.2.3"),
            "commit subject must name formula + version; got: {subject}"
        );
        drop(bare);
    }

    /// `directory:` places the formula in a sub-tree of the tap. Asserts the
    /// pushed object lives at `Formula/mytool.rb`, not the tap root.
    #[test]
    fn publish_to_homebrew_writes_into_configured_directory() {
        let (bare_url, bare) = make_bare_tap("main");
        let mut hb = hb_cfg_local(&bare_url, "main");
        hb.directory = Some("Formula".to_string());
        let mut ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");

        let bare_path = Path::new(&bare_url);
        let formula = tap_show(bare_path, "main", "Formula/mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        // The root path must NOT exist.
        let root = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["cat-file", "-e", "main:mytool.rb"])
                    .current_dir(bare_path);
                cmd
            },
            "git",
        )
        .status;
        assert!(
            !root.success(),
            "formula must live under Formula/, not the tap root"
        );
        drop(bare);
    }

    /// Non-default push branch: `repository.branch` routes the commit onto a
    /// branch other than the tap's seeded default. Asserts the formula landed
    /// on that branch ref.
    #[test]
    fn publish_to_homebrew_pushes_to_configured_branch() {
        let (bare_url, bare) = make_bare_tap("trunk");
        let hb = hb_cfg_local(&bare_url, "trunk");
        let mut ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed);
        let bare_path = Path::new(&bare_url);
        let formula = tap_show(bare_path, "trunk", "mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        drop(bare);
    }

    /// A custom `commit_msg_template` renders into the actual tap commit
    /// subject. Pins that the template (not a hard-coded string) drives the
    /// landed commit message. `render_commit_msg` registers the formula name
    /// as `ProjectName` (it is invoked with `ident.formula_name`) and the
    /// version as `Version`; the Go-style leading dots are stripped by the
    /// template preprocessor before Tera renders, so `.ProjectName` /
    /// `.Version` resolve to those registered vars. (`.Name` is NOT a
    /// registered var — using it would error-render and silently fall back
    /// to the default message.)
    #[test]
    fn publish_to_homebrew_renders_custom_commit_message() {
        let (bare_url, bare) = make_bare_tap("main");
        let mut hb = hb_cfg_local(&bare_url, "main");
        hb.commit_msg_template =
            Some("brew: {{ .ProjectName }} bumped to {{ .Version }}".to_string());
        let mut ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        let subject = git_stdout(Path::new(&bare_url), &["log", "-1", "--pretty=%s", "main"]);
        assert_eq!(
            subject, "brew: mytool bumped to 1.2.3",
            "the custom commit_msg_template must drive the landed commit subject; \
             ProjectName = the formula name, Version = the release version"
        );
        drop(bare);
    }

    /// Idempotent re-publish: running the publisher twice against the same
    /// tap (identical formula content) lands one commit the first time
    /// (Ok(true)) and a no-op the second time (Ok(false)) — the
    /// commit-and-push helper detects the unchanged tree and skips.
    #[test]
    fn publish_to_homebrew_second_run_is_noop() {
        let (bare_url, bare) = make_bare_tap("main");
        let hb = hb_cfg_local(&bare_url, "main");
        let make_ctx = || {
            single_crate_ctx(
                hb.clone(),
                vec![archive(
                    "x86_64-unknown-linux-gnu",
                    "https://e/x.tar.gz",
                    "s",
                )],
            )
        };
        let mut ctx1 = make_ctx();
        assert!(
            publish_to_homebrew(&mut ctx1, "mytool", &quiet_log()).expect("first publish"),
            "first publish must push"
        );
        let mut ctx2 = make_ctx();
        assert!(
            !publish_to_homebrew(&mut ctx2, "mytool", &quiet_log()).expect("second publish"),
            "second publish of identical content must be a no-op (Ok(false))"
        );
        drop(bare);
    }

    /// Workspace lockstep mode: the crate lives only under
    /// `config.workspaces[].crates` (no top-level entry). The publisher must
    /// resolve it via the workspace fallthrough and still land the formula on
    /// the tap — proving per-crate publish is not single-crate-only.
    #[test]
    fn publish_to_homebrew_workspace_crate_lands_formula() {
        let (bare_url, bare) = make_bare_tap("main");
        let hb = hb_cfg_local(&bare_url, "main");
        let config = Config {
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![CrateConfig {
                    name: "mytool".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig {
                        homebrew: Some(hb),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        // No tag → Version is empty; the workspace lockstep path still renders
        // (formula `version ""`), proving config resolution, not version math.
        ctx.artifacts.add(archive(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "s",
        ));
        let pushed =
            publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("workspace publish ok");
        assert!(pushed, "workspace-only crate must still push the formula");
        let formula = tap_show(Path::new(&bare_url), "main", "mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        assert!(formula.contains("https://e/x.tar.gz"), "{formula}");
        drop(bare);
    }

    /// Workspace per-crate mode: two crates each carry their OWN homebrew
    /// block pointing at distinct taps; publishing each lands ITS formula on
    /// ITS tap with ITS own formula name. Proves per-crate config resolution
    /// + per-crate name rendering, not a shared/last-writer-wins config.
    #[test]
    fn publish_to_homebrew_workspace_per_crate_distinct_taps() {
        let (bare_a, holder_a) = make_bare_tap("main");
        let (bare_b, holder_b) = make_bare_tap("main");
        let mut hb_a = hb_cfg_local(&bare_a, "main");
        hb_a.name = Some("alpha".to_string());
        let mut hb_b = hb_cfg_local(&bare_b, "main");
        hb_b.name = Some("beta".to_string());

        let crate_with = |name: &str, hb: HomebrewConfig| CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(hb),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config {
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![crate_with("crate-a", hb_a), crate_with("crate-b", hb_b)],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        // Each crate gets its own archive (artifact crate_name must match).
        let mut art_a = archive("x86_64-unknown-linux-gnu", "https://e/a.tar.gz", "sa");
        art_a.crate_name = "crate-a".to_string();
        let mut art_b = archive("x86_64-unknown-linux-gnu", "https://e/b.tar.gz", "sb");
        art_b.crate_name = "crate-b".to_string();
        ctx.artifacts.add(art_a);
        ctx.artifacts.add(art_b);

        assert!(publish_to_homebrew(&mut ctx, "crate-a", &quiet_log()).expect("publish a"));
        assert!(publish_to_homebrew(&mut ctx, "crate-b", &quiet_log()).expect("publish b"));

        // crate-a's tap carries alpha.rb with crate-a's url; crate-b's tap
        // carries beta.rb with crate-b's url. No cross-contamination.
        let fa = tap_show(Path::new(&bare_a), "main", "alpha.rb");
        assert!(fa.contains("class Alpha < Formula"), "{fa}");
        assert!(fa.contains("https://e/a.tar.gz"), "{fa}");
        let fb = tap_show(Path::new(&bare_b), "main", "beta.rb");
        assert!(fb.contains("class Beta < Formula"), "{fb}");
        assert!(fb.contains("https://e/b.tar.gz"), "{fb}");
        drop(holder_a);
        drop(holder_b);
    }

    /// Same-tap cask co-publish: with a `cask:` block + a darwin DiskImage
    /// artifact, the publisher writes the cask alongside the formula into the
    /// same clone and the single commit covers BOTH files. Asserts both
    /// `mytool.rb` (formula) and `Casks/<cask>.rb` landed on the tap, and the
    /// commit subject reflects the formula+cask kind.
    #[test]
    fn publish_to_homebrew_co_publishes_cask_into_same_tap() {
        let (bare_url, bare) = make_bare_tap("main");
        let mut hb = hb_cfg_local(&bare_url, "main");
        hb.cask = Some(HomebrewCaskConfig {
            name: Some("mytool-cask".to_string()),
            url: Some(HomebrewCaskURL {
                template: Some("https://e/{{ .Version }}/mytool.dmg".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut dmg_meta = HashMap::new();
        dmg_meta.insert("url".to_string(), "https://e/mytool.dmg".to_string());
        dmg_meta.insert("sha256".to_string(), "dmgsha".to_string());
        let dmg = Artifact {
            kind: ArtifactKind::DiskImage,
            path: std::path::PathBuf::from("/tmp/mytool.dmg"),
            name: "mytool.dmg".to_string(),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "mytool".to_string(),
            metadata: dmg_meta,
            size: None,
        };
        let mut ctx = single_crate_ctx(
            hb,
            vec![
                archive("aarch64-apple-darwin", "https://e/arm.tar.gz", "shaarm"),
                dmg,
            ],
        );
        let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed);
        let bare_path = Path::new(&bare_url);
        // Formula at root.
        let formula = tap_show(bare_path, "main", "mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        // Cask under the default Casks/ dir.
        let cask = tap_show(bare_path, "main", "Casks/mytool-cask.rb");
        assert!(cask.contains("cask \"mytool-cask\""), "{cask}");
        // Single commit, formula+cask kind in the subject.
        let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
        assert!(
            subject.contains("cask"),
            "commit subject must reflect the formula+cask kind; got: {subject}"
        );
        drop(bare);
    }

    /// PR path: with `pull_request.enabled = true` (same-repo), the publisher
    /// still commits+pushes the formula to the tap AND attempts a PR. The
    /// formula push (the local effect) must land regardless of the PR outcome.
    ///
    /// Hermetic by construction: a failing `gh` stub forces the PR transport's
    /// `gh_is_available()` probe to false, and no token is configured, so the
    /// PR submission resolves to the `NoneAvailable` fallback IN-PROCESS — it
    /// never issues a live `gh pr create` / GitHub API call against
    /// `myorg/homebrew-tap`. Holds the `PathGuard` for the whole test and is
    /// `#[serial(path_env)]` because it mutates process `PATH` (the shared
    /// `path_env` group serializes it against the `util/pr.rs` and
    /// winget/scoop/krew gh-stub tests crate-wide).
    #[test]
    #[serial(path_env)]
    fn publish_to_homebrew_pr_enabled_still_pushes_formula() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = make_bare_tap("main");
        let mut hb = hb_cfg_local(&bare_url, "main");
        if let Some(repo) = hb.repository.as_mut() {
            repo.pull_request = Some(PullRequestConfig {
                enabled: Some(true),
                ..Default::default()
            });
            // No token configured: with `gh` stubbed absent too, the PR
            // transport has neither path and resolves to NoneAvailable
            // in-process (no network), yet the push already happened.
        }
        let mut ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let pushed = publish_to_homebrew(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(
            pushed,
            "formula push must land even when PR submission is attempted"
        );
        let formula = tap_show(Path::new(&bare_url), "main", "mytool.rb");
        assert!(formula.contains("class Mytool < Formula"), "{formula}");
        drop(bare);
    }

    /// Clone failure surfaces as an `Err`: pointing `git.url` at a path that
    /// is not a git repo makes the clone fail; the publisher must propagate
    /// the error (the tap was never touched), not silently report success.
    #[test]
    fn publish_to_homebrew_clone_failure_errors() {
        let bogus = tempfile::tempdir().expect("bogus dir");
        let bogus_url = bogus.path().to_string_lossy().into_owned();
        let hb = hb_cfg_local(&bogus_url, "main");
        let mut ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let err = publish_to_homebrew(&mut ctx, "mytool", &quiet_log())
            .expect_err("cloning a non-repo path must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("homebrew"),
            "error must name the publisher; got: {msg}"
        );
        drop(bogus);
    }

    // ===================================================================
    // completions / manpages / livecheck / dual-license — the homebrew-core
    // citizen fields the formula previously lacked (the cask already had).
    // Validated against the real ripgrep/fd/bat exemplar idioms.
    // ===================================================================

    /// Single-crate mode: prebuilt completion files + a manpage render as
    /// `bash_completion.install` / `zsh_completion.install` /
    /// `fish_completion.install` / `man1.install` lines INSIDE the install
    /// block — exactly the idiom ripgrep/fd/bat ship.
    #[test]
    fn render_formula_emits_completion_and_manpage_installs_single_crate() {
        let hb = HomebrewConfig {
            completions: Some(HomebrewCaskCompletions {
                bash: Some("completions/mytool.bash".to_string()),
                zsh: Some("completions/_mytool".to_string()),
                fish: Some("completions/mytool.fish".to_string()),
            }),
            manpages: Some(vec!["man/mytool.1".to_string()]),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("bin.install \"mytool\""), "{body}");
        assert!(
            body.contains("bash_completion.install \"completions/mytool.bash\""),
            "{body}"
        );
        assert!(
            body.contains("zsh_completion.install \"completions/_mytool\""),
            "{body}"
        );
        assert!(
            body.contains("fish_completion.install \"completions/mytool.fish\""),
            "{body}"
        );
        assert!(body.contains("man1.install \"man/mytool.1\""), "{body}");
    }

    /// A manpage path ending in `.8` routes to `man8.install`, not `man1`.
    #[test]
    fn render_formula_routes_manpage_to_numbered_section() {
        let hb = HomebrewConfig {
            manpages: Some(vec!["man/mytool.8".to_string()]),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("man8.install \"man/mytool.8\""), "{body}");
    }

    /// `generate_completions_from_executable` (the modern homebrew-core idiom)
    /// renders inside the install block when configured.
    #[test]
    fn render_formula_emits_generate_completions_from_executable() {
        let hb = HomebrewConfig {
            generate_completions_from_executable: Some(HomebrewCaskGeneratedCompletions {
                executable: Some("bin/mytool".to_string()),
                args: Some(vec!["completions".to_string()]),
                shells: Some(vec![
                    "bash".to_string(),
                    "zsh".to_string(),
                    "fish".to_string(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(
            body.contains("generate_completions_from_executable \"bin/mytool\", \"completions\""),
            "{body}"
        );
        assert!(body.contains("shells: [:bash, :zsh, :fish]"), "{body}");
    }

    /// A user-supplied `install:` block OWNS the install body — anodizer must
    /// NOT append auto-completion/man lines (no double-emit).
    #[test]
    fn render_formula_custom_install_does_not_append_completions() {
        let hb = HomebrewConfig {
            install: Some("bin.install \"mytool\"".to_string()),
            completions: Some(HomebrewCaskCompletions {
                bash: Some("c.bash".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(
            !body.contains("bash_completion.install"),
            "custom install owns the block; got:\n{body}"
        );
    }

    /// Default livecheck: a binary tap formula with NO livecheck config emits
    /// `livecheck do\n  skip "Auto-generated on release."\nend`, mirroring the
    /// cask (the archive URL/sha change every release).
    #[test]
    fn render_formula_emits_default_livecheck_skip() {
        let hb = HomebrewConfig::default();
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("livecheck do"), "{body}");
        assert!(
            body.contains("skip \"Auto-generated on release.\""),
            "{body}"
        );
    }

    /// Active livecheck: opting in with `skip: false` + a strategy renders a
    /// `url :stable` / `strategy :github_latest` block, matching ripgrep.
    #[test]
    fn render_formula_emits_active_livecheck_strategy() {
        let hb = HomebrewConfig {
            livecheck: Some(HomebrewLivecheck {
                skip: Some(false),
                strategy: Some("github_latest".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("livecheck do"), "{body}");
        assert!(body.contains("url :stable"), "{body}");
        assert!(body.contains("strategy :github_latest"), "{body}");
        assert!(
            !body.contains("skip \"Auto-generated"),
            "active livecheck must NOT skip; got:\n{body}"
        );
    }

    /// `skip: false` with NO strategy/url/regex is a no-op opt-in: an empty
    /// `livecheck do…end` is invalid Ruby, so the renderer falls back to `skip`
    /// (and warns — the warning is surfaced, not asserted here, since it goes to
    /// stderr). The rendered formula must still carry a valid `skip` block.
    #[test]
    fn render_formula_livecheck_skip_false_without_strategy_falls_back_to_skip() {
        let hb = HomebrewConfig {
            livecheck: Some(HomebrewLivecheck {
                skip: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("livecheck do"), "{body}");
        assert!(
            body.contains("skip \"Auto-generated on release.\""),
            "no-op opt-in must fall back to a valid skip block; got:\n{body}"
        );
    }

    /// Dual-license SPDX (`Apache-2.0 OR MIT`) — the Rust-CLI norm — renders as
    /// `license any_of: ["Apache-2.0", "MIT"]`, NOT an invalid bare string.
    #[test]
    fn render_formula_dual_license_renders_any_of_single_crate() {
        let hb = HomebrewConfig {
            license: Some("Apache-2.0 OR MIT".to_string()),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(
            body.contains("license any_of: [\"Apache-2.0\", \"MIT\"]"),
            "{body}"
        );
        assert!(
            !body.contains("license \"Apache-2.0 OR MIT\""),
            "must not emit the invalid bare-string form; got:\n{body}"
        );
    }

    /// AND dual-license renders `license all_of: [...]`.
    #[test]
    fn render_formula_and_license_renders_all_of() {
        let hb = HomebrewConfig {
            license: Some("Apache-2.0 AND MIT".to_string()),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(
            body.contains("license all_of: [\"Apache-2.0\", \"MIT\"]"),
            "{body}"
        );
    }

    /// A single-id license still renders the plain `license "MIT"` form.
    #[test]
    fn render_formula_single_license_renders_plain_string() {
        let hb = HomebrewConfig {
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let ctx = single_crate_ctx(
            hb,
            vec![archive(
                "x86_64-unknown-linux-gnu",
                "https://e/x.tar.gz",
                "s",
            )],
        );
        let body = render_homebrew_formula_for_crate(&ctx, "mytool", &quiet_log())
            .expect("render")
            .expect("not skipped")
            .formula;
        assert!(body.contains("license \"MIT\""), "{body}");
        assert!(!body.contains("any_of"), "{body}");
    }

    /// Workspace per-crate mode: two crates carry DISTINCT dual licenses +
    /// distinct completion sets. Each formula must render ITS OWN license
    /// `any_of:` and ITS OWN completion installs — proving per-crate resolution
    /// of the new fields, not last-writer-wins.
    #[test]
    fn render_formula_per_crate_distinct_license_and_completions() {
        let hb_a = HomebrewConfig {
            name: Some("alpha".to_string()),
            license: Some("Apache-2.0 OR MIT".to_string()),
            completions: Some(HomebrewCaskCompletions {
                bash: Some("a.bash".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let hb_b = HomebrewConfig {
            name: Some("beta".to_string()),
            license: Some("BSD-3-Clause".to_string()),
            completions: Some(HomebrewCaskCompletions {
                zsh: Some("_beta".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let crate_with = |name: &str, hb: HomebrewConfig| CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(hb),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config {
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![crate_with("crate-a", hb_a), crate_with("crate-b", hb_b)],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut art_a = archive("x86_64-unknown-linux-gnu", "https://e/a.tar.gz", "sa");
        art_a.crate_name = "crate-a".to_string();
        let mut art_b = archive("x86_64-unknown-linux-gnu", "https://e/b.tar.gz", "sb");
        art_b.crate_name = "crate-b".to_string();
        ctx.artifacts.add(art_a);
        ctx.artifacts.add(art_b);

        let body_a = render_homebrew_formula_for_crate(&ctx, "crate-a", &quiet_log())
            .expect("render a")
            .expect("not skipped")
            .formula;
        let body_b = render_homebrew_formula_for_crate(&ctx, "crate-b", &quiet_log())
            .expect("render b")
            .expect("not skipped")
            .formula;

        // crate-a: dual license any_of + bash completion only.
        assert!(
            body_a.contains("license any_of: [\"Apache-2.0\", \"MIT\"]"),
            "a:\n{body_a}"
        );
        assert!(
            body_a.contains("bash_completion.install \"a.bash\""),
            "a:\n{body_a}"
        );
        assert!(
            !body_a.contains("_beta"),
            "no cross-contamination; a:\n{body_a}"
        );

        // crate-b: single license + zsh completion only.
        assert!(body_b.contains("license \"BSD-3-Clause\""), "b:\n{body_b}");
        assert!(!body_b.contains("any_of"), "b:\n{body_b}");
        assert!(
            body_b.contains("zsh_completion.install \"_beta\""),
            "b:\n{body_b}"
        );
        assert!(
            !body_b.contains("a.bash"),
            "no cross-contamination; b:\n{body_b}"
        );
    }
}
