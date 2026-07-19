use super::super::cask_scope::generate_cask_from_context;
use super::super::commit_msg::render_commit_msg;
use super::super::formula::{FormulaOptions, generate_formula_with_opts};
use anodizer_core::config::HomebrewConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

use super::*;

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
) -> Result<Option<super::super::cask_scope::CaskGenResult>> {
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
    let directory = super::super::resolve_cask_directory(cask_cfg.directory.as_deref(), ctx)?;
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
                "## Formula\n- **Name**: {}\n- **Version**: {}\n\n## Cask\n- **Name**: {}\n- **Version**: {}\n\n{}",
                formula_name,
                version,
                cask_name,
                version,
                crate::util::SUBMITTED_BY_FOOTER
            ),
        )
    } else {
        (
            format!("Update {} formula to {}", formula_name, version),
            format!(
                "## Formula\n- **Name**: {}\n- **Version**: {}\n\n{}",
                formula_name,
                version,
                crate::util::SUBMITTED_BY_FOOTER
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
pub(super) fn render_formula_inner(
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
        livecheck: super::super::formula::render_livecheck(hb_cfg.livecheck.as_ref(), log),
        // Render the `license` stanza from the parsed SPDX expression so a dual
        // license (`Apache-2.0 OR MIT`) becomes `license any_of: [...]` rather
        // than an invalid bare string. `None` when no license resolved → the
        // template omits the stanza.
        license_stanza: meta
            .license
            .as_deref()
            .and_then(super::super::formula::render_formula_license),
    };

    let archive_data = collect_archive_entries(ctx, hb_cfg, crate_name, &version, log)?;
    let archives: Vec<(&str, &str, &str)> = archive_data
        .iter()
        .map(|(t, u, s)| (t.as_str(), u.as_str(), s.as_str()))
        .collect();

    let formula_name = meta.formula_name.as_str();
    let formula = generate_formula_with_opts(
        &super::super::formula::FormulaCore {
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
        &super::super::formula::FormulaCode {
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

    let branch = crate::util::resolve_branch_or_versioned(
        ctx,
        hb_cfg_owned.repository.as_ref(),
        formula_name,
        &version,
    );

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
