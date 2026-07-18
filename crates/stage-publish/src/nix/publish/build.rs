use std::path::Path;

use anodizer_core::config::{NixConfig, NixDependency};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{self, OsArtifact};

use super::super::generate::{SourceRootEntry, nix_system};
use super::super::hashing::hex_sha256_to_nix_base32;
use anodizer_core::elf::is_dynamically_linked;

// ---------------------------------------------------------------------------
// Artifact + archive helpers
// ---------------------------------------------------------------------------

/// The nix system for a platform artifact, or `None` when it is not
/// nix-installable.
///
/// Wraps [`nix_system`] with the genuine-macOS check the raw `(os, arch)`
/// mapping cannot make on its own: `map_target` classifies every `*-apple-*`
/// triple as `os = "darwin"`, folding `aarch64-apple-ios` / `-tvos` /
/// `-watchos` in with real macOS. A nix darwin package built from a watchOS
/// archive is a failure-hiding emission — a `nix build` on `aarch64-darwin`
/// would fetch a binary that cannot run there. So a `darwin`-classified
/// artifact is nix-eligible only when its triple is genuine macOS
/// ([`is_macos`]); this mirrors homebrew's `is_macos || is_linux` artifact
/// filter. Linux is already precise (`map_target` never mislabels a non-Linux
/// triple `linux`), so it passes through untouched.
///
/// [`is_macos`]: anodizer_core::target::is_macos
pub(super) fn nix_system_for_artifact(a: &OsArtifact) -> Option<String> {
    if a.os == "darwin" && !anodizer_core::target::is_macos(&a.target) {
        return None;
    }
    nix_system(&a.os, &a.arch)
}

/// Gathers all Linux/Darwin platform artifacts for the crate, applying
/// the configured ID filter and `amd64_variant` (defaulting to `v1`).
pub(super) fn collect_platform_artifacts(
    ctx: &Context,
    crate_name: &str,
    nix_cfg: &NixConfig,
) -> anyhow::Result<Vec<OsArtifact>> {
    let ids_filter = nix_cfg.ids.as_deref();
    let amd64_variant = nix_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        None,
    )
}

/// Builds the `(nix_system, download_url, base32_hash)` triples that
/// feed into the Tera template. Bails out before emitting an
/// unverifiable derivation if any nix-system artifact is missing its
/// `sha256` metadata. Warns and falls back to raw hex if the base32
/// conversion errors.
pub(super) fn build_archive_tuples(
    all_artifacts: &[OsArtifact],
    nix_cfg: &NixConfig,
    crate_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<Vec<(String, String, String)>> {
    if let Some(empty) = all_artifacts
        .iter()
        .find(|a| nix_system_for_artifact(a).is_some() && a.sha256.is_empty())
    {
        anyhow::bail!(
            "nix: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated Nix derivation \
             would embed an empty `sha256 = \"\";`, which `nix-build` rejects \
             (the fetchurl fixed-output derivation cannot verify the source). \
             Check dist/artifacts.json for the archive entry's metadata.sha256 \
             and re-run `task release` from a clean dist/ if the field is \
             absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }

    let url_template = nix_cfg.url_template.as_deref();
    // Multiple artifacts can map to one nix system (e.g. an Archive and an
    // UploadableBinary for the same target, or several archive formats). The
    // derivation's `urlMap`/`shaMap`/`src` and `meta.platforms` must each carry
    // exactly one entry per system, so dedup by nix system here at the source.
    // First occurrence wins (deterministic), matching the artifact ordering
    // (`Archive` kind precedes `UploadableBinary`); without this the BTreeMap
    // downstream collapsed urlMap last-writer-wins while `meta.platforms`
    // triplicated, an inconsistency that also broke output reproducibility.
    let mut seen_systems = std::collections::HashSet::new();
    let archives: Vec<(String, String, String)> = all_artifacts
        .iter()
        .filter_map(|a| {
            let system = nix_system_for_artifact(a)?;
            if !seen_systems.insert(system.clone()) {
                return None;
            }
            let download_url = if let Some(tmpl) = url_template {
                util::render_url_template(tmpl, crate_name, version, &a.arch, &a.os)
            } else {
                a.url.clone()
            };
            let nix_hash = match hex_sha256_to_nix_base32(&a.sha256) {
                Ok(h) => h,
                Err(e) => {
                    log.warn(&format!(
                        "failed to convert SHA256 to nix base32 for {}: {}; using raw hex",
                        a.url, e
                    ));
                    a.sha256.clone()
                }
            };
            Some((system, download_url, nix_hash))
        })
        .collect();

    if archives.is_empty() {
        anyhow::bail!(
            "nix: no Linux/Darwin archive artifacts found for '{}'",
            crate_name
        );
    }
    Ok(archives)
}

/// De-duplicates the dependency attribute names while preserving the
/// declaration order — these become the derivation function arguments.
pub(super) fn unique_dep_args(deps: &[NixDependency]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    deps.iter()
        .filter(|d| seen.insert(d.name.clone()))
        .map(|d| d.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Install + sourceRoot + dyn-link detection
// ---------------------------------------------------------------------------

/// Builds the lines that compose the Nix `installPhase`. Falls back to
/// the auto-generated `mkdir -p $out/bin; cp …` block when no custom
/// `install` script is configured. Appends `wrapProgram` invocations
/// for OS-filtered dependencies when `makeWrapper` is needed.
pub(super) fn build_install_lines(
    nix_cfg: &NixConfig,
    crate_cfg: &anodizer_core::config::CrateConfig,
    name: &str,
    deps: &[NixDependency],
    needs_make_wrapper: bool,
) -> Vec<String> {
    if let Some(ref custom_install) = nix_cfg.install {
        let mut lines: Vec<String> = custom_install.lines().map(|l| l.to_string()).collect();
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        return lines;
    }

    let mut lines = vec!["mkdir -p $out/bin".to_string()];
    let bin_names = collect_binary_names(crate_cfg, name);
    for bin in &bin_names {
        lines.push(format!("cp -vr ./{bin} $out/bin/{bin}"));
        lines.push(format!("chmod +x $out/bin/{bin}"));
    }
    // Install shell completions / man pages the archive bundles. The archive
    // stage lays completions under `completions/` and man pages under
    // `man/man1/` (the `*Config::DEFAULT_DST` dirs) inside every archive, so
    // when the crate configures either block, the unpacked sourceRoot carries
    // those files and `installShellCompletion` / `installManPage` route them
    // into the derivation's `$out` rather than dropping them. Gated on the
    // archive config actually requesting them — mirrors how ripgrep/fd install
    // their completions/man in nixpkgs.
    lines.extend(build_completion_install_lines(crate_cfg, &bin_names));
    lines.extend(build_manpage_install_lines(crate_cfg));
    if let Some(ref extra) = nix_cfg.extra_install {
        lines.extend(extra.lines().map(|l| l.to_string()));
    }
    if needs_make_wrapper && let Some(wrap_line) = build_wrap_program_line(deps, name) {
        lines.push(wrap_line);
    }
    lines
}

/// Build `installShellCompletion` lines for any archive entry that bundles
/// completions. The archive stage writes one file per shell into the entry's
/// completions dir (default `completions/`) named per clap convention
/// (`<bin>` / `_<bin>` / `<bin>.fish`), so `installShellCompletion
/// --cmd <bin> --bash … --zsh … --fish …` picks each up by its on-disk name.
/// Only bash/zsh/fish have an `installShellFiles` flag, so other shells the
/// user generated are left in the archive (still distributed). Returns an
/// empty vec when no archive entry configures completions.
pub(super) fn build_completion_install_lines(
    crate_cfg: &anodizer_core::config::CrateConfig,
    bin_names: &[String],
) -> Vec<String> {
    use anodizer_core::config::{ArchivesConfig, completion_filename};
    let ArchivesConfig::Configs(cfgs) = &crate_cfg.archives else {
        return Vec::new();
    };
    let primary_bin = bin_names.first().map(String::as_str).unwrap_or("");
    let mut lines = Vec::new();
    for cfg in cfgs {
        let Some(comp) = cfg.completions.as_ref() else {
            continue;
        };
        if matches!(comp.mode(), anodizer_core::config::GenMode::None) {
            continue;
        }
        let dst = comp.resolved_dst();
        let dir = dst.strip_suffix('/').unwrap_or(dst);
        // Only the three shells `installShellCompletion` natively flags.
        let mut flags = String::new();
        for (shell, flag) in [("bash", "--bash"), ("zsh", "--zsh"), ("fish", "--fish")] {
            if comp
                .resolved_shells()
                .iter()
                .any(|s| s.eq_ignore_ascii_case(shell))
            {
                let file = completion_filename(primary_bin, shell);
                flags.push_str(&format!(" {flag} {dir}/{file}"));
            }
        }
        if !flags.is_empty() {
            lines.push(format!("installShellCompletion --cmd {primary_bin}{flags}"));
        }
    }
    lines
}

/// Build `installManPage` lines for any archive entry that bundles man pages.
/// The archive stage writes man files into the entry's manpages dir (default
/// `man/man1/`), so a glob over that dir installs whatever the archive ships.
/// Returns an empty vec when no archive entry configures man pages.
pub(super) fn build_manpage_install_lines(
    crate_cfg: &anodizer_core::config::CrateConfig,
) -> Vec<String> {
    use anodizer_core::config::{ArchivesConfig, GenMode};
    let ArchivesConfig::Configs(cfgs) = &crate_cfg.archives else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cfg in cfgs {
        let Some(man) = cfg.manpages.as_ref() else {
            continue;
        };
        if matches!(man.mode(), GenMode::None) {
            continue;
        }
        let dst = man.resolved_dst();
        let dir = dst.strip_suffix('/').unwrap_or(dst);
        if seen.insert(dir.to_string()) {
            lines.push(format!("installManPage {dir}/*"));
        }
    }
    lines
}

/// Pulls binary names from each configured build, de-duplicated in
/// declaration order. Falls back to the derivation name when no builds
/// are configured.
pub(super) fn collect_binary_names(
    crate_cfg: &anodizer_core::config::CrateConfig,
    name: &str,
) -> Vec<String> {
    let mut names: Vec<String> = crate_cfg
        .builds
        .as_ref()
        .map(|builds| {
            builds
                .iter()
                .filter_map(|b| b.binary.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    if names.is_empty() {
        names.push(name.to_string());
    }
    names
}

/// Builds the single `wrapProgram … --prefix PATH : ${lib.makeBinPath …}`
/// line that splices dependencies into the wrapped binary's PATH.
/// Partitions deps into darwin-only, linux-only, and all-OS buckets so
/// the generated expression uses `lib.optionals std…isDarwin` /
/// `…isLinux` guards. Returns `None` when no deps survive the partition.
pub(super) fn build_wrap_program_line(deps: &[NixDependency], name: &str) -> Option<String> {
    let all_os_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.is_none())
        .map(|d| d.name.as_str())
        .collect();
    let darwin_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.as_deref() == Some("darwin"))
        .map(|d| d.name.as_str())
        .collect();
    let linux_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.as_deref() == Some("linux"))
        .map(|d| d.name.as_str())
        .collect();

    let mut list_parts: Vec<String> = Vec::new();
    if !darwin_deps.is_empty() {
        let items = darwin_deps.join(" ");
        list_parts.push(format!("lib.optionals stdenvNoCC.isDarwin [ {items} ]"));
    }
    if !linux_deps.is_empty() {
        let items = linux_deps.join(" ");
        list_parts.push(format!("lib.optionals stdenvNoCC.isLinux [ {items} ]"));
    }
    if !all_os_deps.is_empty() {
        let items = all_os_deps.join(" ");
        list_parts.push(format!("[ {items} ]"));
    }

    if list_parts.is_empty() {
        return None;
    }
    let joined = list_parts.join(" ++\n      ");
    Some(format!(
        "wrapProgram $out/bin/{name} --prefix PATH : ${{lib.makeBinPath (\n      {joined}\n    )}}"
    ))
}

/// Resolves the derivation's `sourceRoot` from each archive config's
/// `wrap_in_directory`. Returns a single `Some(root)` when every Nix
/// system maps to the same value, otherwise yields a per-system
/// `SourceRootEntry` list sorted by system identifier.
pub(super) fn resolve_source_roots(
    crate_cfg: &anodizer_core::config::CrateConfig,
    all_artifacts: &[OsArtifact],
    name: &str,
    version: &str,
) -> (Option<String>, Option<Vec<SourceRootEntry>>) {
    let default_stem = format!("{}-{}", name, version);
    let archive_cfgs = match &crate_cfg.archives {
        anodizer_core::config::ArchivesConfig::Configs(cfgs) => cfgs.clone(),
        anodizer_core::config::ArchivesConfig::Disabled => vec![],
    };

    let mut per_system: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for art in all_artifacts {
        if let Some(system) = nix_system_for_artifact(art) {
            let wrap_dir = archive_cfgs
                .iter()
                .find(|cfg| match (&art.id, &cfg.id) {
                    (Some(aid), Some(cid)) => aid == cid,
                    (_, None) if archive_cfgs.len() == 1 => true,
                    _ => false,
                })
                .or_else(|| archive_cfgs.first())
                .and_then(|cfg| {
                    cfg.wrap_in_directory
                        .as_ref()
                        .and_then(|w| w.directory_name(&default_stem))
                })
                .unwrap_or_else(|| ".".to_string());
            per_system.insert(system, wrap_dir);
        }
    }

    let unique_roots: std::collections::HashSet<&str> =
        per_system.values().map(|s| s.as_str()).collect();

    if unique_roots.len() <= 1 {
        let single = per_system
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        (Some(single), None)
    } else {
        let mut entries: Vec<SourceRootEntry> = per_system
            .into_iter()
            .map(|(system, root)| SourceRootEntry { system, root })
            .collect();
        entries.sort_by(|a, b| a.system.cmp(&b.system));
        (None, Some(entries))
    }
}

/// Returns `true` if any binary artifact for the crate is dynamically
/// linked. Prefers the build-stage metadata flag `DynamicallyLinked` to
/// avoid redundant disk I/O; falls back to direct ELF inspection for
/// artifacts that lack the marker.
pub(super) fn detect_dynamically_linked(ctx: &Context, crate_name: &str) -> anyhow::Result<bool> {
    let binary_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Binary, crate_name);
    for a in &binary_artifacts {
        if let Some(v) = a.metadata.get("DynamicallyLinked") {
            if v == "true" {
                return Ok(true);
            }
            continue;
        }
        // A registered binary we cannot inspect must fail the nix publish, not
        // silently drop autoPatchelfHook and ship a broken derivation.
        if is_dynamically_linked(&a.path)
            .with_context(|| format!("inspecting {} for ELF dynamic linking", a.path.display()))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Formatter + commit/push helpers
// ---------------------------------------------------------------------------

/// Runs the configured `alejandra` / `nixfmt` formatter against the
/// generated derivation. Formatting is opt-in (no `formatter` set is a
/// no-op, matching GoReleaser), but once a formatter IS configured it is
/// MANDATORY: a missing binary, a non-zero exit, or an unrecognized name
/// each `bail!`s so the unformatted derivation is never committed/pushed
/// to the external nix repo.
///
/// This is INTENTIONALLY stricter than GoReleaser, whose `nix.go::format`
/// only warns on failure — the "no unformatted push" requirement justifies
/// the divergence; the opt-in gating (format only when a formatter is set)
/// still matches GR.
pub(super) fn run_formatter(nix_cfg: &NixConfig, nix_file: &Path, log: &StageLogger) -> Result<()> {
    let Some(ref formatter) = nix_cfg.formatter else {
        return Ok(());
    };
    match formatter.as_str() {
        "alejandra" | "nixfmt" => {}
        _ => {
            anyhow::bail!(
                "nix: unknown formatter '{}' (expected alejandra or nixfmt)",
                formatter
            );
        }
    }

    // Detect-and-fail-loud (no runtime auto-install) — consistent with
    // cosign/syft being required-present. The CI base image
    // (anodizer-action `install:`) provisions the formatter. A genuine probe
    // error (e.g. permission denied) surfaces as itself rather than the
    // misleading "not found on PATH" remedy.
    match anodizer_core::tool_detect::runs(formatter) {
        anodizer_core::tool_detect::ToolProbe::Available => {}
        anodizer_core::tool_detect::ToolProbe::Unavailable => {
            anyhow::bail!(
                "nix: formatter '{formatter}' not found on PATH — install it \
                 (anodizer-action install: list / CI base image) so the generated \
                 derivation is formatted before push"
            );
        }
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            anyhow::bail!("nix: could not probe formatter '{formatter}' availability ({e})");
        }
    }

    let nix_file_str = nix_file.to_string_lossy();
    let output = std::process::Command::new(formatter)
        .arg(&*nix_file_str)
        .output()
        .with_context(|| format!("nix: spawn formatter '{formatter}'"))?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "nix: {formatter} formatting failed for {} (exit {code}); \
             refusing to push an unformatted derivation\n{stderr}{stdout}",
            nix_file.display()
        );
    }
    log.status(&format!("formatted nix derivation with {formatter}"));
    Ok(())
}

/// Renders the commit message, commits + pushes the nix expression,
/// then optionally opens a PR. Returns `true` when an actual push
/// reached the remote (matches `publish_to_nix`'s rollback contract).
#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_publish(
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    repo_path: &Path,
    files: &[&str],
    name: &str,
    version: &str,
    repo_owner: &str,
    repo_name: &str,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let previous_tag = ctx
        .template_vars()
        .get("PreviousTag")
        .cloned()
        .unwrap_or_default();
    let commit_msg = crate::homebrew::render_commit_msg_with_prev(
        nix_cfg.commit_msg_template.as_deref(),
        name,
        version,
        &previous_tag,
        "nix",
        log,
        ctx.render_is_strict(),
    )?;
    let commit_opts = util::resolve_commit_opts(ctx, nix_cfg.commit_author.as_ref(), log)?;
    let branch = util::resolve_branch_or_versioned(ctx, nix_cfg.repository.as_ref(), name, version);
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        files,
        &commit_msg,
        branch.as_deref(),
        "nix",
        &commit_opts,
        log,
    )?;

    // Clone the repository config so `maybe_submit_pr` no longer
    // borrows from `ctx.config` (via `nix_cfg`). NLL then drops the
    // immutable borrow, making the subsequent `&mut ctx` call legal.
    let repo_for_pr = nix_cfg.repository.clone();
    let pr_branch = branch.as_deref().unwrap_or("main").to_string();
    let pr_outcome = util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &util::PrOrigin {
            repo_owner,
            repo_name,
            branch_name: &pr_branch,
            // Nix publishes commit directly to the expression repo
            // branch; the optional PR is informational. The
            // winget/krew/cask `update_existing_pr:` flag has no
            // analogue on `NixConfig` because there's no real
            // "blocked queue" to recover from here.
            update_existing_pr: false,
        },
        &format!("Update {} to {}", name, version),
        &format!(
            "## Package\n- **Name**: {}\n- **Version**: {}\n\n{}",
            name,
            version,
            crate::util::SUBMITTED_BY_FOOTER
        ),
        "nix",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );

    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Nix expression pushed to {}/{} for '{}'",
                repo_owner, repo_name, crate_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, nix expression for '{}' already up to date",
                crate_name
            ));
        }
    }

    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }

    Ok(outcome.is_pushed())
}
