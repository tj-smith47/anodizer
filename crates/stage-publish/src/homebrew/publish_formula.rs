//! `publish_to_homebrew` — per-crate formula (and optional same-tap cask)
//! publisher.
use super::cask::generate_cask_from_context;
use super::commit_msg::render_commit_msg;
use super::formula::{FormulaOptions, generate_formula_with_opts};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};

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
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;

    let hb_cfg = publish
        .homebrew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("homebrew: no homebrew config for '{}'", crate_name))?;

    // Check skip_upload before doing any work.
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

    // GoReleaser Pro parity: fall back to project-level `metadata.*` when the
    // homebrew config's own field is unset. `metadata.description` / `homepage`
    // / `license` is consumed here (config-must-wire).
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
    let homepage_rendered = hb_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .map(|h| ctx.render_template(h).unwrap_or_else(|_| h.to_string()));

    // Build template vars so install/test/extra_install/post_install can use
    // {{ name }} and {{ version }} (GoReleaser parity).
    let mut tmpl_vars = TemplateVars::new();
    tmpl_vars.set("name", crate_name);
    tmpl_vars.set("version", &version);

    // Auto-generate multi-binary install lines from ExtraBinaries artifact
    // metadata when no explicit install is configured (GoReleaser parity:
    // installs() reads artifact.ExtraBinaries to produce sorted bin.install lines).
    let install_raw = if let Some(ref custom_install) = hb_cfg.install {
        custom_install.clone()
    } else {
        // Collect binary names from archive metadata across all matching artifacts.
        let mut bin_names = std::collections::BTreeSet::new();
        for art in ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name)
        {
            for name in art.extra_binaries() {
                bin_names.insert(name);
            }
        }
        // For UploadableBinary artifacts, use "filename" => "binary_name" syntax.
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
    let test_block = template::render(&test_raw, &tmpl_vars).unwrap_or_else(|_| test_raw.clone());

    // Template-render extra_install and post_install if provided.
    let extra_install_rendered = hb_cfg
        .extra_install
        .as_deref()
        .map(|s| template::render(s, &tmpl_vars).unwrap_or_else(|_| s.to_string()));
    let post_install_rendered = hb_cfg
        .post_install
        .as_deref()
        .map(|s| template::render(s, &tmpl_vars).unwrap_or_else(|_| s.to_string()));

    // Derive GitHub slug (owner/repo) for homepage fallback.
    let github_slug = _crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));

    let opts = FormulaOptions {
        homepage: homepage_rendered.as_deref(),
        github_slug,
        dependencies: hb_cfg.dependencies.as_deref(),
        conflicts: hb_cfg.conflicts.as_deref(),
        caveats: hb_cfg.caveats.as_deref(),
        extra_install: extra_install_rendered.as_deref(),
        post_install: post_install_rendered.as_deref(),
        download_strategy: hb_cfg.download_strategy.as_deref(),
        url_headers: hb_cfg.url_headers.as_deref(),
        custom_require: hb_cfg.custom_require.as_deref(),
        custom_block: hb_cfg.custom_block.as_deref(),
        plist: hb_cfg.plist.as_deref(),
        service: hb_cfg.service.as_deref(),
    };

    // Collect Archive and Binary artifacts for this crate to build the formula entries.
    // GoReleaser supports both UploadableArchive and UploadableBinary types here.
    // Apply IDs + amd64_variant/arm_variant filter.
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
        .filter_map(|a| {
            let target = a.target.as_deref().unwrap_or("");
            // When url_template is set, render it to produce the download URL;
            // otherwise use the artifact metadata URL (from the release stage).
            let url = if let Some(tmpl) = hb_cfg.url_template.as_deref() {
                let (os, arch) = anodizer_core::target::map_target(target);
                crate::util::render_url_template_with_ctx(ctx, tmpl, a.name(), &version, &arch, &os)
            } else {
                a.metadata.get("url")?.to_string()
            };
            let sha256 = a.metadata.get("sha256")?.to_string();
            let format = a.metadata.get("format").cloned().unwrap_or_default();
            Some((target.to_string(), url, sha256, format))
        })
        .collect();

    // Disambiguate: when ids: is unset and multiple archives share an OS/arch
    // key, prefer .tar.gz over other formats (most-conventional Homebrew archive).
    let archive_data =
        disambiguate_homebrew_archives(raw_archive_data, ids_filter.is_some(), crate_name, log)?;

    //
    // — empty archive set after filtering produces a broken formula with
    // empty url/sha256. Bail with an actionable error that cites the filters
    // the user would need to adjust to get a match.
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

    let archives: Vec<(&str, &str, &str)> = archive_data
        .iter()
        .map(|(t, u, s)| (t.as_str(), u.as_str(), s.as_str()))
        .collect();

    // Use name override if set, otherwise crate name; render through template engine.
    let formula_name_raw = hb_cfg.name.as_deref().unwrap_or(crate_name);
    let formula_name_rendered = ctx
        .render_template(formula_name_raw)
        .unwrap_or_else(|_| formula_name_raw.to_string());
    let formula_name = formula_name_rendered.as_str();

    let formula = generate_formula_with_opts(
        &super::formula::FormulaCore {
            name: formula_name,
            version: &version,
            description: &description,
            license: license.as_deref().unwrap_or(""),
        },
        &archives,
        &super::formula::FormulaCode {
            install: &install,
            test: &test_block,
        },
        &opts,
    )?;

    // Clone tap repo, write formula, commit, push.
    let tmp_dir = tempfile::tempdir().context("homebrew: create temp dir")?;
    let repo_path = tmp_dir.path();

    let token = crate::util::resolve_repo_token(
        ctx,
        hb_cfg.repository.as_ref(),
        Some("HOMEBREW_TAP_TOKEN"),
    );
    crate::util::clone_repo(
        hb_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "homebrew",
        log,
    )?;

    // Determine formula directory (GoReleaser parity: `directory` field).
    let directory = hb_cfg.directory.clone().unwrap_or_default();
    let formula_dir = if directory.is_empty() {
        repo_path.to_path_buf()
    } else {
        repo_path.join(&directory)
    };
    std::fs::create_dir_all(&formula_dir)
        .with_context(|| format!("homebrew: create formula dir {}", formula_dir.display()))?;

    let formula_path = formula_dir.join(format!("{}.rb", formula_name));
    std::fs::write(&formula_path, &formula)
        .with_context(|| format!("homebrew: write formula {}", formula_path.display()))?;

    log.status(&format!(
        "wrote Homebrew formula: {}",
        formula_path.display()
    ));

    // If a cask config is present, generate and write the cask file into the
    // same clone so we commit and push everything in one shot (avoiding a
    // redundant second clone/commit/push cycle).
    let mut cask_name_for_log: Option<String> = None;
    let mut cask_path_lossy: Option<String> = None;

    if let Some(cask_cfg) = hb_cfg.cask.as_ref() {
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
        } else {
            let cask_result = generate_cask_from_context(ctx, crate_name, hb_cfg, cask_cfg)?;

            let casks_dir = repo_path.join("Casks");
            std::fs::create_dir_all(&casks_dir).with_context(|| {
                format!("homebrew cask: create Casks dir {}", casks_dir.display())
            })?;

            let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
            std::fs::write(&cask_path, &cask_result.content).with_context(|| {
                format!("homebrew cask: write cask file {}", cask_path.display())
            })?;

            log.status(&format!("wrote Homebrew cask: {}", cask_path.display()));
            cask_path_lossy = Some(cask_path.to_string_lossy().into_owned());
            cask_name_for_log = Some(cask_result.cask_name);
        }
    }

    // Build the list of files to commit: always the formula, plus the cask if present.
    let formula_lossy = formula_path.to_string_lossy();
    let mut files_to_commit: Vec<&str> = vec![&formula_lossy];
    if let Some(ref cask_lossy) = cask_path_lossy {
        files_to_commit.push(cask_lossy);
    }

    // Render commit message from template or use default.
    let kind = if cask_name_for_log.is_some() {
        "formula and cask"
    } else {
        "formula"
    };
    let commit_msg = render_commit_msg(
        hb_cfg.commit_msg_template.as_deref(),
        formula_name,
        &version,
        kind,
    );

    let commit_opts = crate::util::resolve_commit_opts(ctx, hb_cfg.commit_author.as_ref());
    let branch = crate::util::resolve_branch(hb_cfg.repository.as_ref());
    let outcome = crate::util::commit_and_push_with_opts(
        repo_path,
        &files_to_commit,
        &commit_msg,
        branch,
        "homebrew",
        &commit_opts,
    )?;
    match outcome {
        crate::util::CommitOutcome::Pushed => {
            if let Some(ref cask_name) = cask_name_for_log {
                log.status(&format!(
                    "Homebrew tap {}/{} updated with formula '{}' and cask '{}'",
                    repo_owner, repo_name, formula_name, cask_name
                ));
            } else {
                log.status(&format!(
                    "Homebrew tap {}/{} updated for '{}'",
                    repo_owner, repo_name, crate_name
                ));
            }
        }
        crate::util::CommitOutcome::NoChanges => {
            log.status("homebrew: nothing to push, formula already up to date");
        }
    }

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.unwrap_or("main");
    let (pr_title, pr_body) = if let Some(ref cask_name) = cask_name_for_log {
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
    // Clone the repository config so the `maybe_submit_pr` call no
    // longer borrows from `ctx.config` (via `hb_cfg`). NLL then drops
    // the `hb_cfg` / `publish` immutable borrows, which makes the
    // subsequent `&mut ctx` call legal. The config is a handful of
    // strings — clone cost is trivial.
    let repo_for_pr = hb_cfg.repository.clone();

    let pr_outcome = crate::util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &crate::util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
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

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(matches!(outcome, crate::util::CommitOutcome::Pushed))
}
