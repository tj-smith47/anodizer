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
/// `metadata.*` per GoReleaser Pro parity.
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
) -> ResolvedMetadata {
    let description_raw = hb_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let license = hb_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license())
        .map(|l| ctx.render_template(l).unwrap_or_else(|_| l.to_string()));
    let homepage = hb_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .map(|h| ctx.render_template(h).unwrap_or_else(|_| h.to_string()));
    let formula_name_raw = hb_cfg.name.as_deref().unwrap_or(crate_name);
    let formula_name = ctx
        .render_template(formula_name_raw)
        .unwrap_or_else(|_| formula_name_raw.to_string());
    ResolvedMetadata {
        description,
        license,
        homepage,
        formula_name,
    }
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
/// (GoReleaser parity).
fn render_install_and_test_blocks(
    ctx: &Context,
    hb_cfg: &HomebrewConfig,
    crate_name: &str,
    version: &str,
) -> RenderedFormulaCode {
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
                    bin_names.insert(format!("{}\" => \"{}", art.name(), bin));
                } else {
                    bin_names.insert(bin);
                }
            }
        }
        if bin_names.is_empty() {
            format!("bin.install \"{}\"", crate_name)
        } else {
            bin_names
                .into_iter()
                .map(|name| format!("bin.install \"{}\"", name))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let install =
        template::render(&install_raw, &tmpl_vars).unwrap_or_else(|_| install_raw.clone());
    let test_raw = hb_cfg
        .test
        .clone()
        .unwrap_or_else(|| format!("system \"#{{bin}}/{}\", \"--version\"", crate_name));
    let test = template::render(&test_raw, &tmpl_vars).unwrap_or_else(|_| test_raw.clone());

    let extra_install = hb_cfg
        .extra_install
        .as_deref()
        .map(|s| template::render(s, &tmpl_vars).unwrap_or_else(|_| s.to_string()));
    let post_install = hb_cfg
        .post_install
        .as_deref()
        .map(|s| template::render(s, &tmpl_vars).unwrap_or_else(|_| s.to_string()));
    RenderedFormulaCode {
        install,
        test,
        extra_install,
        post_install,
    }
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
    let amd64_variant = hb_cfg.amd64_variant.as_deref().or(Some("v1"));
    // GoReleaser defaults Goarm to "6" for Homebrew (brew.go:85)
    let arm_variant = hb_cfg.arm_variant.as_deref().or(Some("6"));
    let mut all_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name);
    all_artifacts.extend(ctx.artifacts.by_kind_and_crate(
        anodizer_core::artifact::ArtifactKind::UploadableBinary,
        crate_name,
    ));
    // Collect as (target, url, sha256, format) so the disambiguator can prefer
    // .tar.gz when multiple archives match the same OS/arch and ids: is unset.
    let raw_archive_data: Vec<(String, String, String, String)> = all_artifacts
        .iter()
        // OnlyReplacingUnibins: exclude universal binaries that didn't replace
        // single-arch variants (GoReleaser parity).
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
            if arch == "amd64"
                && let Some(want) = amd64_variant
            {
                return a.metadata.get("amd64_variant").is_none_or(|v| v == want);
            }
            if arch.starts_with("arm")
                && arch != "arm64"
                && let Some(want) = arm_variant
            {
                return a.metadata.get("arm_variant").is_none_or(|v| v == want);
            }
            true
        })
        .map(|a| {
            let target = a.target.as_deref().unwrap_or("");
            // When url_template is set, render it to produce the download URL;
            // otherwise use the artifact metadata URL (from the release stage).
            let url = if let Some(tmpl) = hb_cfg.url_template.as_deref() {
                let (os, arch) = anodizer_core::target::map_target(target);
                crate::util::render_url_template_with_ctx(ctx, tmpl, a.name(), version, &arch, &os)
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
        let amd_hint = amd64_variant.unwrap_or("<default v1>");
        let arm_hint = arm_variant.unwrap_or("<default 6>");
        anyhow::bail!(
            "homebrew: no archives matched filters for '{crate_name}' — \
             formula would have empty url/sha256. Check your archive \
             configuration and homebrew filters ({ids_hint}, \
             amd64_variant={amd_hint}, arm_variant={arm_hint}). At least one \
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
        hb_cfg.repository.as_ref(),
        tap.repo_owner,
        tap.repo_name,
        token.as_deref(),
        tap.repo_path,
        "homebrew",
        log,
    )?;

    // Determine formula directory (GoReleaser parity: `directory` field).
    // Empty string means "tap repo root" — the `is_empty()` branch below
    // uses `repo_path` directly without joining, so the empty default is the
    // documented no-subdirectory mode (most Homebrew taps put formulae at
    // the root); GoReleaser brew.go behaves the same way.
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
        "wrote Homebrew formula: {}",
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
    let Some(cask_cfg) = hb_cfg.cask.as_ref() else {
        return Ok(CaskInTapOutcome::default());
    };
    if crate::util::should_skip_upload(cask_cfg.skip_upload.as_ref(), ctx, log) {
        log.status(&format!(
            "homebrew cask: skipping upload for '{}' (skip_upload={})",
            crate_name,
            cask_cfg
                .skip_upload
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("")
        ));
        return Ok(CaskInTapOutcome::default());
    }
    let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg)?;

    let casks_dir = repo_path.join("Casks");
    std::fs::create_dir_all(&casks_dir)
        .with_context(|| format!("homebrew cask: create Casks dir {}", casks_dir.display()))?;

    let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
    std::fs::write(&cask_path, &cask_result.content)
        .with_context(|| format!("homebrew cask: write cask file {}", cask_path.display()))?;

    log.status(&format!("wrote Homebrew cask: {}", cask_path.display()));
    Ok(CaskInTapOutcome {
        cask_name: Some(cask_result.cask_name),
        cask_path: Some(cask_path),
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
    let mut files_to_commit: Vec<&str> = vec![&formula_lossy];
    if let Some(ref cl) = cask_lossy {
        files_to_commit.push(cl);
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
    );

    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref());
    let outcome = crate::util::commit_and_push_with_opts(
        tap.repo_path,
        &files_to_commit,
        &commit_msg,
        branch,
        "homebrew",
        &commit_opts,
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
                "homebrew: nothing to push, formula for '{}' already up to date",
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
    );

    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }
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

    if crate::util::should_skip_upload(hb_cfg.skip_upload.as_ref(), ctx, log) {
        log.status(&format!(
            "homebrew: skipping upload for '{}' (skip_upload={})",
            crate_name,
            hb_cfg
                .skip_upload
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("")
        ));
        return Ok(false);
    }

    let proceed = anodizer_core::config::evaluate_if_condition(
        hb_cfg.if_condition.as_deref(),
        &format!("homebrew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "homebrew: skipping '{}' — `if` condition evaluated falsy",
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

    let meta = resolve_homebrew_metadata(ctx, &hb_cfg_owned, crate_name);
    let code = render_install_and_test_blocks(ctx, &hb_cfg_owned, crate_name, &version);

    let opts = FormulaOptions {
        homepage: meta.homepage.as_deref(),
        github_slug,
        dependencies: hb_cfg_owned.dependencies.as_deref(),
        conflicts: hb_cfg_owned.conflicts.as_deref(),
        caveats: hb_cfg_owned.caveats.as_deref(),
        extra_install: code.extra_install.as_deref(),
        post_install: code.post_install.as_deref(),
        download_strategy: hb_cfg_owned.download_strategy.as_deref(),
        url_headers: hb_cfg_owned.url_headers.as_deref(),
        custom_require: hb_cfg_owned.custom_require.as_deref(),
        custom_block: hb_cfg_owned.custom_block.as_deref(),
        plist: hb_cfg_owned.plist.as_deref(),
        service: hb_cfg_owned.service.as_deref(),
    };

    let archive_data = collect_archive_entries(ctx, &hb_cfg_owned, crate_name, &version, log)?;
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

    let branch = crate::util::resolve_branch(hb_cfg_owned.repository.as_ref());

    let outcome = commit_files_to_tap(
        ctx,
        &hb_cfg_owned,
        &ident,
        &tap,
        &formula_path,
        &cask,
        branch,
        log,
    )?;

    let pr_branch = branch.unwrap_or("main");
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
    use crate::util::CommitOutcome;

    #[test]
    fn commit_outcome_is_pushed() {
        assert!(CommitOutcome::Pushed.is_pushed());
        assert!(!CommitOutcome::NoChanges.is_pushed());
    }
}
