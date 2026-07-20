use super::*;

/// Which check axes the selected publish surface leaves in scope, resolved
/// once in [`VerifyReleaseStage::run`] and read per crate.
pub(crate) struct AxisScope {
    /// github-release survives the operator selection (asset axis in scope).
    pub(crate) github_selected: bool,
    /// This run itself uploaded the release assets (github-release runs in
    /// this leg). When `false` — the pre-submitter gate verifying a release
    /// an earlier leg published — byte comparison is exempted for
    /// publish-surface-dependent assets: the combined checksum manifests,
    /// whose bytes the uploading leg rewrote at upload time to fold in
    /// publish-time evidence (docker image digests) this leg never produces.
    /// Every other asset stays byte-checked and the missing-asset diff stays
    /// fully strict, so the exemption cannot widen into a bypass.
    pub(crate) assets_published_by_this_run: bool,
    /// At least one OS-package carrier is selected (libc + smoke in scope).
    pub(crate) os_pkg_selected: bool,
    /// `install_smoke:` is configured.
    pub(crate) smoke_enabled: bool,
    /// Docker probe succeeded (only probed when smoke is in surface).
    pub(crate) docker_ok: bool,
}

/// Mutable cross-crate accumulation for the verify loop.
#[derive(Default)]
pub(crate) struct VerifyRun {
    /// Every post-publish defect found, across all crates and axes.
    pub(crate) issues: Vec<String>,
    /// The resolved install-smoke strategy is emitted once per run, on the
    /// first smoke job, so a CI operator can tell a slow copy path (dind
    /// without a shared work dir) from a fast bind-mount path.
    smoke_strategy_logged: bool,
}

/// Per-crate tally of what each check axis ACTUALLY examined.
///
/// An axis being enabled and in-surface proves only that it was in scope;
/// these counters prove it inspected ≥1 artifact — the difference between
/// "verified" and "had nothing to verify". The aggregation site refuses to
/// stamp a green verdict off all-zero counters, so a run that proved nothing
/// never fabricates passing evidence.
#[derive(Default)]
pub(crate) struct CrateVerifyOutcome {
    /// Published releases whose asset set was fetched and diffed/byte-checked.
    assets_inspected: usize,
    /// Packages whose embedded ELF was extracted and glibc-evaluated, or
    /// whose read failed (the failure pushed an issue, so it still counts as
    /// a real inspection).
    libc_inspected: usize,
    /// Packages actually submitted to the install-smoke matrix.
    smoke_inspected: usize,
}

impl CrateVerifyOutcome {
    /// Fold another crate's tally into this one.
    pub(crate) fn absorb(&mut self, other: &Self) {
        self.assets_inspected += other.assets_inspected;
        self.libc_inspected += other.libc_inspected;
        self.smoke_inspected += other.smoke_inspected;
    }

    /// Whether any axis examined at least one artifact.
    pub(crate) fn any_inspected(&self) -> bool {
        self.assets_inspected > 0 || self.libc_inspected > 0 || self.smoke_inspected > 0
    }
}

pub(crate) fn verify_one_crate(
    ctx: &Context,
    log: &StageLogger,
    rt: &tokio::runtime::Runtime,
    cfg: &VerifyReleaseConfig,
    crate_cfg: &CrateConfig,
    scope: &AxisScope,
    run: &mut VerifyRun,
) -> Result<CrateVerifyOutcome> {
    let mut outcome = CrateVerifyOutcome::default();
    // The caller filters to crates carrying a release block; if absent there
    // is no published release to verify, so skip this crate rather than panic.
    let Some(release_cfg) = crate_cfg.release.as_ref() else {
        return Ok(outcome);
    };
    let issues = &mut run.issues;

    // (a) asset existence + content ------------------------------------------
    if cfg.assert_assets_enabled() && scope.github_selected {
        match rt.block_on(anodizer_stage_release::fetch_published_assets(
            ctx,
            release_cfg,
            crate_cfg,
        )) {
            Ok(Some(published_assets)) => {
                outcome.assets_inspected += 1;
                let published: Vec<String> =
                    published_assets.iter().map(|a| a.name.clone()).collect();
                let produced = produced_asset_names(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    release_cfg.exclude.as_deref(),
                );
                // Config-derived expectations (signatures / SBOMs). A
                // derivation error is itself a finding — never a silent pass.
                let derived = match config_expected_asset_names(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    release_cfg.exclude.as_deref(),
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        issues.push(format!(
                            "could not derive expected signature/SBOM assets \
                                 from config for crate '{}': {e:#}",
                            crate_cfg.name
                        ));
                        Vec::new()
                    }
                };
                let mut all_expected = produced.clone();
                all_expected.extend(derived);
                all_expected.sort();
                all_expected.dedup();

                let diff = diff_assets(&all_expected, &published);
                let produced_set: std::collections::BTreeSet<&str> =
                    produced.iter().map(String::as_str).collect();
                let (missing_produced, missing_derived): (Vec<&String>, Vec<&String>) = diff
                    .missing
                    .iter()
                    .partition(|name| produced_set.contains(name.as_str()));
                if !missing_produced.is_empty() {
                    issues.push(format!(
                        "{1} produced artifact(s) missing from the published \
                         release for crate '{0}': {2}",
                        crate_cfg.name,
                        missing_produced.len(),
                        missing_produced
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !missing_derived.is_empty() {
                    issues.push(format!(
                        "{1} signature/SBOM asset(s) required by the resolved \
                         signs/sboms config were never uploaded for crate '{0}' \
                         (the producing stage registered no such artifact): {2}",
                        crate_cfg.name,
                        missing_derived.len(),
                        missing_derived
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !diff.has_missing() {
                    log.verbose(&format!(
                        "crate '{}' all {} asset(s) present \
                         ({} config-derived)",
                        crate_cfg.name,
                        all_expected.len(),
                        all_expected.len() - produced.len()
                    ));
                }
                if !diff.orphan.is_empty() {
                    log.verbose(&format!(
                        "crate '{}' {} orphan asset(s) on release (advisory): {}",
                        crate_cfg.name,
                        diff.orphan.len(),
                        diff.orphan.join(", ")
                    ));
                }

                // Every expected asset that IS present also gets a byte-level
                // check: stored size (and digest, when GitHub exposes one)
                // must match the local artifact.
                let content = verify_published_contents(
                    ctx,
                    log,
                    crate_cfg,
                    release_cfg,
                    &all_expected,
                    &published_assets,
                    scope.assets_published_by_this_run,
                    issues,
                );
                let present = all_expected.len() - diff.missing.len();
                if !diff.has_missing() && content.issue_count == 0 {
                    let digest_note = match content.digest_unverified {
                        0 => "sizes+digests match".to_string(),
                        k => format!("sizes match ({k} digest(s) unverifiable)"),
                    };
                    log.status(&format!(
                        "github: crate '{}' {present}/{} assets present, {digest_note}",
                        crate_cfg.name,
                        all_expected.len(),
                    ));
                }
            }
            Ok(None) => {
                log.verbose(&format!(
                    "skipped asset-existence for crate '{}' — \
                     no GitHub release configured",
                    crate_cfg.name
                ));
            }
            Err(e) => {
                // Failing to fetch the live release is itself a post-publish
                // signal worth surfacing, not a silent skip.
                issues.push(format!(
                    "could not fetch published release assets for crate '{}': {e}",
                    crate_cfg.name
                ));
            }
        }
    }

    // (c) libc-ceiling ------------------------------------------------------
    // `glibc_check_enabled()` is true only when a ceiling is set; the
    // `if let` keeps that an invariant the type system enforces rather than an
    // unwrap that could panic if the predicate ever diverges from the field.
    if cfg.glibc_check_enabled()
        && scope.os_pkg_selected
        && let Some(ceiling) = cfg.glibc_ceiling.as_deref()
    {
        for (path, name, _) in linux_packages(ctx, &crate_cfg.name) {
            if PackageType::from_filename(&name).is_none() {
                continue;
            }
            if check_one_package_libc(log, &crate_cfg.name, path, ceiling, issues) {
                outcome.libc_inspected += 1;
            }
        }
    }

    // (b) install smoke-test ------------------------------------------------
    // `smoke_enabled` is derived from `install_smoke.is_some()`; the `if let`
    // ties the config presence to its enablement flag without an unwrap.
    // Gating on the OS-package surface here (not only at the caller's
    // docker-probe site) keeps the loop's precondition local instead of
    // relying on the caller having zeroed `docker_ok` when out of surface.
    if scope.smoke_enabled
        && scope.os_pkg_selected
        && scope.docker_ok
        && let Some(smoke_cfg) = cfg.install_smoke.as_ref()
    {
        let binary = crate_binary_name(crate_cfg);
        for (path, name, target) in linux_packages(ctx, &crate_cfg.name) {
            let Some(pt) = PackageType::from_filename(&name) else {
                continue;
            };
            outcome.smoke_inspected += 1;
            let image = match pt {
                PackageType::Deb => smoke_cfg.deb_image(),
                PackageType::Rpm => smoke_cfg.rpm_image(),
                PackageType::Apk => smoke_cfg.apk_image(),
            };
            let job = SmokeJob {
                image: image.to_string(),
                package_type: pt,
                host_pkg_path: path.to_string_lossy().to_string(),
                pkg_name: name.clone(),
                binary: binary.clone(),
                platform: smoke::job_platform(target.as_deref()),
            };
            if !run.smoke_strategy_logged {
                log.verbose(&format!(
                    "using install-smoke strategy {}",
                    smoke::strategy_label(&job.image)
                ));
                run.smoke_strategy_logged = true;
            }
            match smoke::run_smoke(&job) {
                Ok(SmokeOutcome::Passed) => {
                    log.verbose(&format!(
                        "crate '{}' smoke passed ({name} on {image})",
                        crate_cfg.name
                    ));
                }
                Ok(SmokeOutcome::Failed { detail }) => {
                    issues.push(format!(
                        "install smoke-test failed for crate '{}' ({name} on {image}): {detail}",
                        crate_cfg.name
                    ));
                }
                Err(e) => {
                    issues.push(format!(
                        "install smoke-test could not run for crate '{}' ({name}): {e}",
                        crate_cfg.name
                    ));
                }
            }
        }
    }

    Ok(outcome)
}
