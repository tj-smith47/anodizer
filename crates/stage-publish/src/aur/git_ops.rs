use super::*;

/// Build the `(arch, download_url, sha256)` source tuples for the
/// PKGBUILD `source_<arch>=` / `sha256sums_<arch>=` arrays. Filters
/// `ctx.artifacts` to Linux archives matching `aur.ids` + the
/// hardcoded `amd64_variant`/`arm_variant=7` rules, validates that at
/// least one archive matched and that every match carries a non-empty
/// sha256, then dedupes by PKGBUILD architecture (`x86_64`, `aarch64`,
/// `i686`, `armv7h`) keeping the first match per arch.
pub(crate) fn aur_build_sources(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    version: &str,
) -> Result<Vec<(String, String, String)>> {
    // Find Linux artifacts for the AUR package, applying IDs + amd64_variant filter.
    // arm_variant is hardcoded to "7" for AUR (no config option).
    let ids_filter = aur_cfg.ids.as_deref();
    let amd64_variant = aur_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let linux_artifacts = util::find_artifacts_by_os_with_variant(
        ctx,
        crate_name,
        "linux",
        ids_filter,
        Some(amd64_variant),
        Some("7"),
    )?;

    // An empty linux-archive set produces a PKGBUILD with placeholder URL and
    // empty sha256 that users would have to hand-fix. Hard-fail with an
    // actionable error instead.
    if linux_artifacts.is_empty() {
        let ids_hint = ids_filter
            .map(|ids| format!("ids={ids:?}"))
            .unwrap_or_else(|| "ids=<none>".to_string());
        // Hint from the raw config, not the folded filter value, so a
        // defaulted selector reads `<default …>` while a configured one
        // prints plainly.
        let amd64_hint = aur_cfg.amd64_variant.map_or("<default v1>", |v| v.as_str());
        anyhow::bail!(
            "aur: no linux archives matched filters for '{crate_name}' — \
             PKGBUILD would have placeholder URL and empty sha256. Check your \
             archive configuration and aur filters ({ids_hint}, \
             amd64_variant={amd64_hint}, arm_variant=7 [hardcoded]). At least \
             one linux Archive artifact must match."
        );
    }

    // The PKGBUILD `sha256sums_<arch>=('...')` array is consumed by
    // `makepkg`'s integrity check (per
    // https://wiki.archlinux.org/title/PKGBUILD#sha256sums). An empty
    // hash string `('')` is silently accepted by makepkg but disables the
    // verification — installers would download an unverified tarball.
    // Bail before emitting a PKGBUILD whose hashes cannot prove
    // tarball integrity.
    if let Some(empty) = linux_artifacts.iter().find(|a| a.sha256.is_empty()) {
        anyhow::bail!(
            "aur: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated PKGBUILD would \
             emit `sha256sums_<arch>=('')`, which disables makepkg's \
             integrity check and ships an unverified tarball. Check \
             dist/artifacts.json for the archive entry's metadata.sha256 \
             and re-run `task release` from a clean dist/ if the field is \
             absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }

    let url_template = aur_cfg.url_template.as_deref();
    // Deduplicate by architecture — AUR -bin packages expect one source per
    // architecture. When multiple artifacts share the same arch (e.g.
    // multiple linux-amd64 archives), keep only the first match.
    let mut seen_arches = std::collections::HashSet::new();
    let mut sources: Vec<(String, String, String)> = Vec::new();
    for a in &linux_artifacts {
        // Map the artifact GOARCH to its pacman name. An unknown architecture
        // must HARD-FAIL: silently relabeling it (the historical
        // `_ => "x86_64"` fallthrough) would map a non-x86 tarball under
        // `source_x86_64=`/`sha256sums_x86_64=`, so a user on that arch
        // downloads the right PKGBUILD but installs a binary that cannot run.
        let pkgbuild_arch = crate::aur_arch::goarch_to_pacman_arch(&a.arch).map_err(|e| {
            anyhow::anyhow!(
                "aur: {} for crate '{}' (artifact url '{}', os={}). The AUR -bin \
                 PKGBUILD cannot name this architecture for pacman; emitting it \
                 would mislabel the tarball under the wrong `arch=()` entry and \
                 ship a binary that will not run on the target host. Restrict \
                 the AUR archive set (e.g. `publish.aur.ids`) to architectures \
                 Arch Linux supports (x86_64, aarch64, armv7h, i686), or extend \
                 the arch mapping.",
                e,
                crate_name,
                a.url,
                a.os,
            )
        })?;
        if seen_arches.insert(pkgbuild_arch.to_string()) {
            let download_url = if let Some(tmpl) = url_template {
                // Extract the archive filename from the artifact URL (or
                // path fallback) so {{ .ArtifactName }} resolves to the
                // actual archive filename, not the crate name (which has
                // no extension and would leave ArtifactName unset).
                let artifact_filename = std::path::Path::new(&a.url)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                util::render_url_template_with_ctx_and_artifact(
                    ctx,
                    tmpl,
                    crate_name,
                    artifact_filename.as_deref(),
                    version,
                    pkgbuild_arch,
                    "linux",
                )
            } else {
                a.url.clone()
            };
            sources.push((pkgbuild_arch.to_string(), download_url, a.sha256.clone()));
        }
    }

    Ok(sources)
}

/// Clone the AUR git repo into `repo_path`. When either `aur.private_key`
/// or `aur.git_ssh_command` is set the SSH clone path is taken; otherwise
/// falls back to a plain (no-auth-header) clone. AUR has no bearer-token
/// flow so the auth-aware variant is never invoked with credentials.
pub(crate) fn aur_clone_repo(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    git_url: &str,
    repo_path: &std::path::Path,
    log: &StageLogger,
) -> Result<()> {
    if aur_cfg.private_key.is_some() || aur_cfg.git_ssh_command.is_some() {
        // `private_key` / `git_ssh_command` may be templated
        // (`{{ .Env.AUR_SSH_KEY }}`). Render before the SSH clone, or the
        // literal template text is written to the key file and ssh fails
        // with "error in libcrypto".
        let rendered_key = match aur_cfg.private_key.as_deref() {
            Some(pk) => Some(util::render_or_warn(ctx, log, "aur.private_key", pk)?),
            None => None,
        };
        let rendered_ssh = match aur_cfg.git_ssh_command.as_deref() {
            Some(sc) => Some(util::render_or_warn(ctx, log, "aur.git_ssh_command", sc)?),
            None => None,
        };
        util::clone_repo_ssh(
            git_url,
            rendered_key.as_deref(),
            rendered_ssh.as_deref(),
            repo_path,
            "aur",
            log,
        )
    } else {
        util::clone_repo_with_auth(git_url, None, repo_path, "aur", log)
    }
}

/// Resolve the output directory inside the cloned repo, optionally
/// creating a subdirectory rendered from `aur.directory`. The directory
/// template is rendered first, then the path is created.
pub(crate) fn aur_resolve_output_dir(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    repo_path: &std::path::Path,
    log: &StageLogger,
) -> Result<std::path::PathBuf> {
    if let Some(ref dir) = aur_cfg.directory {
        let rendered_dir = util::render_or_warn(ctx, log, "aur.directory", dir)?;
        let d = repo_path.join(&rendered_dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("aur: create directory {}", d.display()))?;
        Ok(d)
    } else {
        Ok(repo_path.to_path_buf())
    }
}

/// Write `PKGBUILD`, the optional `.install` file, and `.SRCINFO` into
/// `output_dir`. `install_filename` is precomputed by the caller as
/// `<package_name minus trailing -bin>.install`; the `.install` file
/// is only emitted when `install_content` is `Some`. Status lines
/// mirror the formerly-inline `log.status` calls.
pub(crate) fn aur_write_package_files(
    output_dir: &std::path::Path,
    pkgbuild: &str,
    srcinfo: &str,
    install_filename: &str,
    install_content: Option<&str>,
    log: &StageLogger,
) -> Result<()> {
    let pkgbuild_path = output_dir.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, pkgbuild)
        .with_context(|| format!("aur: write PKGBUILD {}", pkgbuild_path.display()))?;
    log.status(&format!("wrote AUR PKGBUILD {}", pkgbuild_path.display()));

    if let Some(content) = install_content {
        let install_path = output_dir.join(install_filename);
        std::fs::write(&install_path, content).with_context(|| {
            format!("aur: write {} {}", install_filename, install_path.display())
        })?;
        log.status(&format!(
            "wrote AUR install file {}",
            install_path.display()
        ));
    }

    let srcinfo_path = output_dir.join(".SRCINFO");
    std::fs::write(&srcinfo_path, srcinfo)
        .with_context(|| format!("aur: write .SRCINFO {}", srcinfo_path.display()))?;
    log.status(&format!("wrote AUR .SRCINFO {}", srcinfo_path.display()));

    Ok(())
}

/// Commit the staged files in `repo_path` and push to AUR `master`.
/// Returns `true` when the push delivered a new commit, `false` when
/// `commit_and_push_with_opts` reports `NoChanges` (nothing to ship,
/// repo already up to date).
pub(crate) fn aur_commit_and_push(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    repo_path: &std::path::Path,
    package_name: &str,
    version: &str,
    git_url: &str,
    log: &StageLogger,
) -> Result<bool> {
    let commit_msg = crate::homebrew::render_commit_msg(
        aur_cfg.commit_msg_template.as_deref(),
        package_name,
        version,
        "package",
        log,
        ctx.render_is_strict(),
    )?;
    let commit_opts = util::resolve_commit_opts(ctx, aur_cfg.commit_author.as_ref(), log)?;
    // AUR repositories are always on `master`. Pin the push branch via the
    // shared [`AUR_REPO_BRANCH`] constant so the publish and rollback
    // paths can never drift (e.g. one renamed to `main`).
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(AUR_REPO_BRANCH),
        "aur",
        &commit_opts,
        log,
    )?;
    let pushed = match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "AUR package '{}' pushed to {}",
                package_name, git_url
            ));
            true
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, aur package '{}' already up to date",
                package_name
            ));
            false
        }
    };
    Ok(pushed)
}
