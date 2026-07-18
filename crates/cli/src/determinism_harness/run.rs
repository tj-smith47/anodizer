use super::*;

impl Harness {
    /// Apply the external-tool availability gate to `effective_stages`,
    /// returning the stages whose backing tool is reachable.
    ///
    /// Covers every tool-gated producer: the installer family (`wix`,
    /// `rpmbuild`, `makensis`, …), the Linux package formats (`appimage`,
    /// `flatpak`), and the config-resolved stages (`msi`'s WiX version,
    /// `upx`'s binary — see [`Self::config_tools`]). The pipeline would
    /// otherwise fail mid-run at `Command::new("wix")` /
    /// `Command::new("rpmbuild")`, or — for `upx` — silently warn-skip the
    /// stage at runtime, surfacing a confusing error or false coverage
    /// instead of an honest "tool absent". A stage the operator EXPLICITLY
    /// typed into `--stages` (tracked in [`Self::explicit_stages`]) whose
    /// tool is missing is a HARD ERROR (a silent skip would be false
    /// determinism coverage). A host-default stage (resolved into
    /// [`Self::stages`] but never typed) whose tool is missing — e.g.
    /// `appimage` without `linuxdeploy` on the Linux default — warns and
    /// drops the stage so the harness stays usable. Stages with no tool
    /// requirement pass through.
    ///
    /// Under [`Self::require_tools`] (CI's `--require-tools`) the hard-fail
    /// contract widens to the ENTIRE resolved set: a host-default OS-native
    /// producer with a missing tool fails the run too, closing the silent-
    /// under-coverage hole that the removed per-shard `det_stages` naming
    /// used to guard.
    ///
    /// `probe` is injected so the hard-fail wiring is unit-testable
    /// without depending on which tools the host has installed.
    pub(crate) fn gate_installer_stages<P>(
        &self,
        effective_stages: &[StageId],
        probe: P,
    ) -> Result<Vec<StageId>>
    where
        P: Fn(&str) -> bool,
    {
        let gate = installer_detect::filter_available_with_probe(
            effective_stages,
            &self.config_tools,
            probe,
        );
        // The hard-fail set: under `--require-tools` (CI) the WHOLE resolved
        // stage set must have its tools present, so a host-default OS-native
        // producer with a missing tool fails the run. Otherwise only the
        // operator-typed explicit stages hard-fail; host-default stages warn-
        // skip below so dev boxes without the full toolchain stay usable.
        let hard_fail_set: &[StageId] = if self.require_tools {
            effective_stages
        } else {
            &self.explicit_stages
        };
        let hard_failed = gate.explicitly_skipped(hard_fail_set);
        if !hard_failed.is_empty() {
            anyhow::bail!(installer_detect::missing_tool_error(
                &hard_failed,
                self.require_tools
            ));
        }
        // Routed through the harness logger (not a bare eprintln) so
        // `-q` silences these like every other harness line. Only
        // non-hard-fail (host-default, no `--require-tools`) skips reach
        // here; a hard-fail set member already errored above.
        let warn_log = StageLogger::new("check-determinism", self.verbosity);
        for (stage, tool) in &gate.skipped {
            warn_log.warn(&format!(
                "skipped stage `{}` for this run — `{}` is not on PATH \
                 (no artifacts emitted)",
                stage.as_str(),
                tool
            ));
        }
        Ok(gate.available)
    }

    /// Drive the harness end-to-end and return the populated report.
    ///
    /// Does NOT write the report — the CLI dispatcher is responsible for
    /// serializing the returned `DeterminismReport` and exiting non-zero
    /// when `drift_count > 0`.
    pub fn run(&self) -> Result<DeterminismReport> {
        let mut per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>> =
            Vec::with_capacity(self.runs as usize);

        // Preserve-dist + production-keys → skip Sign in the harness.
        //
        // When the workflow plans to ship the harness's output via the
        // publish-only path (`--preserve-dist=<path>` set on the harness;
        // `COSIGN_KEY` / `GPG_PRIVATE_KEY` exported on the runner), the
        // harness's ephemeral signatures would land in the preserved dist
        // and have to be stripped before re-signing with production keys.
        // Cleaner to never write them: skip the Sign stage entirely.
        //
        // KNOWN COVERAGE GAP: byte-stability of the Sign stage is no
        // longer exercised in CI when this branch fires. Acceptable
        // tradeoff — the `harness_signing` unit tests already pin the
        // SDE-based key derivation (cosign-keygen + GPG `--faked-system-
        // time`) so the deterministic-keys property has direct coverage,
        // and the production sign stage is exercised by every release.
        let skip_sign_for_preserve = self.preserve_dist.is_some()
            && (std::env::var_os("COSIGN_KEY").is_some()
                || std::env::var_os("GPG_PRIVATE_KEY").is_some());
        let effective_stages: Vec<StageId> = if skip_sign_for_preserve {
            self.stages
                .iter()
                .copied()
                .filter(|s| *s != StageId::Sign)
                .collect()
        } else {
            self.stages.clone()
        };

        let effective_stages =
            self.gate_installer_stages(&effective_stages, installer_detect::host_tool_probe)?;

        require_c_toolchain(
            self.targets.as_deref().unwrap_or(&[]),
            anodizer_core::determinism::host_is_windows_msvc(),
            anodizer_core::tool_detect::on_path,
        )?;

        // Provision once: both runs must sign with identical key
        // material, otherwise even byte-deterministic GPG signatures
        // would diverge. Skipped when `skip_sign_for_preserve` is set
        // (no Sign stage → no keys needed).
        let signing_keys: Option<EphemeralSigningKeys> =
            if effective_stages.contains(&StageId::Sign) {
                Some(anodizer_core::harness_signing::provision_ephemeral_keys(
                    self.sde,
                )?)
            } else {
                None
            };

        // Default to <repo_root>/.det-worktrees/ — keeps the harness
        // off `/tmp` (which is tmpfs on many distros and exhausts fast
        // when the cargo target dir lives inside the worktree). CI
        // (GitHub Actions) sets RUNNER_TEMP to a disk-backed path
        // outside the repo, so honor that when present.
        let worktree_root = std::env::var_os("RUNNER_TEMP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.repo_root.join(".det-worktrees"));
        let _ = std::fs::create_dir_all(&worktree_root);
        // PID-suffix the worktree so parallel harness invocations
        // (cargo test running multiple determinism integration tests
        // concurrently) don't collide on the same path. WITHIN one
        // invocation every run reuses the same path — that's the
        // load-bearing invariant for /Brepro and UTF-16 cargo-registry
        // paths embedded into binaries (drift otherwise cascades from
        // a 2-byte path diff). Across invocations the path must be
        // unique because git worktree add refuses a populated target.
        let worktree_path =
            worktree_root.join(format!("anodize-determinism-{}", std::process::id()));

        // Shared, lock-pinned CARGO_HOME for the WHOLE invocation, hoisted
        // OUT of the per-run worktree (which the loop wipes each iteration).
        // This is the load-bearing change that makes every rebuild network-
        // free: the run-0 prefetch warms this registry cache once, and it
        // survives into runs 1..N instead of being re-downloaded from clean
        // each time. Determinism-safe to share — `.crate` tarballs + their
        // extracted sources are content-addressed and pinned by `Cargo.lock`,
        // so byte-identical no matter which run fetched them. Only COMPILED
        // output must stay per-run-fresh, and it does: `CARGO_TARGET_DIR`
        // lives inside the worktree and is wiped with it every iteration.
        let shared_cargo_home =
            worktree_root.join(format!("anodize-determinism-cargo-{}", std::process::id()));
        std::fs::create_dir_all(&shared_cargo_home)?;

        // When crate_name is set, anchor the preserved dist into a
        // per-crate subdir so parallel crate releases (each invoking
        // the harness independently) can merge into one `dist/` root
        // without colliding on `context.json` / `artifacts.json`. All
        // downstream writers (preserve_dist_tree, preserve_raw_binaries,
        // write_preserved_dist_context) accept this dest directly and
        // emit into it as-is; the subdir is computed once here so the
        // path stays consistent across the three calls below.
        let effective_preserve_dest: Option<std::path::PathBuf> =
            self.preserve_dist.as_ref().map(|base| {
                if let Some(ref name) = self.crate_name {
                    base.join(name)
                } else {
                    base.clone()
                }
            });

        // Emits the per-run delimiter bullets inside the dispatcher's
        // `Checking determinism` section; the child subprocess's own
        // sections nest beneath each bullet via the inherited log depth
        // (see `determinism_runner::build_subprocess_command`).
        let log = StageLogger::new("check-determinism", self.verbosity);

        // Largest MEASURED peak consumption observed across prior runs, the
        // bound the headroom guard projects forward for run-1..N. `None`
        // until run-0's sampler reports (and stays `None` if the probe is
        // unavailable on this host). The net-vs-peak distinction is the
        // whole point: a between-runs net delta misses the mid-dmg peak.
        let mut max_prior_peak: Option<u64> = None;
        // One-shot latch so a permanently-broken free-space probe warns
        // ONCE (loud enough to notice in CI history) and then degrades
        // quietly, rather than spamming a warn per run.
        let mut probe_gap_warned = false;

        for run_idx in 0..self.runs {
            log.detail(&format!("run {} of {}", run_idx + 1, self.runs));
            // Probe free space BEFORE this run touches disk and apply the
            // fail-fast headroom guard. run-1..N are gated on the largest
            // measured peak of any prior run (× safety factor); run-0 has
            // no prior peak and is gated by the absolute floor alone.
            // `worktree_root` is the parent of the per-run worktree, so it
            // backs the same volume — probe it (it exists; the per-run
            // `worktree_path` does not until `Worktree::add`).
            let free_before = anodizer_core::disk::available_bytes(&worktree_root);
            if free_before.is_none() && !probe_gap_warned {
                log.warn(&format!(
                    "free-space probe unavailable on {} — determinism disk-headroom guard \
                     disabled for this invocation (a permanently-failing probe would otherwise \
                     silently skip the guard for an entire CI history)",
                    worktree_root.display()
                ));
                probe_gap_warned = true;
            }
            self.guard_run_headroom(&log, run_idx, &worktree_root, free_before, max_prior_peak)?;

            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            if run_idx == 0 {
                // Warm the shared registry cache ONCE — online, with retries
                // — before any child build runs. Every rebuild below is
                // sealed offline (`CARGO_NET_OFFLINE=true` in the child env),
                // so this is the single, survivable network touch-point. The
                // man-page `before:` hook (`cargo run … man`) was merely the
                // FIRST cargo call to hit the empty per-run cache and flake on
                // a transient `Could not resolve host: index.crates.io`; a warm
                // shared cache plus the offline seal removes that live-crates.io
                // dependency from the gate entirely.
                log.verbose("prefetching dependencies into shared cargo home (online, retried)");
                anodizer_core::determinism_runner::prefetch_deps(
                    worktree.path(),
                    &shared_cargo_home,
                )
                .context("prefetching dependencies for the determinism harness")?;
            }
            let env =
                self.build_isolated_env(&worktree, &shared_cargo_home, signing_keys.as_ref())?;
            // Sample free space throughout the build + produce stages so the
            // mid-dmg PEAK (the actual ENOSPC moment) is measured, not the
            // post-reclaim net residue. On an error path the sampler's
            // `Drop` reaps the thread; we only read its minimum on success.
            let sampler = anodizer_core::disk::FreeSpaceSampler::start(
                &worktree_root,
                anodizer_core::disk::DEFAULT_SAMPLE_INTERVAL,
            );
            self.run_build_pipeline(worktree.path(), &env, &effective_stages)
                .with_context(|| format!("building pipeline for determinism run {}", run_idx))?;
            if effective_stages.contains(&StageId::CargoPackage) {
                self.run_cargo_package(worktree.path(), &env)
                    .with_context(|| {
                        format!(
                            "running cargo-package stage for determinism run {}",
                            run_idx
                        )
                    })?;
            }
            if effective_stages.contains(&StageId::Docker) {
                // Fork on operator INTENT, not mere set membership: the Linux
                // host default now includes `docker`, so `self.stages` holds
                // it on a bare run too. An explicitly typed `--stages=…,docker`
                // (tracked in `explicit_stages`) — or any docker under CI's
                // `--require-tools` — hard-fails when buildx is unreachable; a
                // plain host-default docker warn-skips so the harness stays
                // usable where Docker is absent. A gate that silently skips a
                // required stage is false coverage, hence the hard-error
                // contract for that case below.
                let docker_explicitly_requested =
                    self.require_tools || self.explicit_stages.contains(&StageId::Docker);
                self.run_docker_stage(worktree.path(), &env, docker_explicitly_requested)
                    .with_context(|| {
                        format!("running docker stage for determinism run {}", run_idx)
                    })?;
            }
            // Stop the sampler now the disk high-water mark has passed. The
            // peak = free-before − min-free-observed; fold it into
            // `max_prior_peak` so run-(idx+1)'s guard is gated on the
            // largest real peak seen so far. Emitted at verbose so the
            // first CI run surfaces run-0's true number (B1.3 / W1).
            let min_free_during = sampler.stop();
            if let (Some(before), Some(min_free)) = (free_before, min_free_during) {
                let peak = anodizer_core::disk::RunPeak {
                    free_before: before,
                    min_free_during: min_free,
                };
                let consumed = peak.consumed_bytes();
                let dist_size = anodizer_core::disk::dir_size_bytes(&worktree.path().join("dist"));
                log.verbose(&format!(
                    "disk peak run {}: consumed {} (min free {}, worktree dist {})",
                    run_idx + 1,
                    anodizer_core::disk::format_gib(consumed),
                    anodizer_core::disk::format_gib(min_free),
                    anodizer_core::disk::format_gib(dist_size),
                ));
                max_prior_peak = Some(max_prior_peak.map_or(consumed, |m| m.max(consumed)));
            }
            let artifacts = discover_artifacts(worktree.path())?;
            // `--inject-drift=<stage>` (test-harness gated): mutate the
            // first artifact of the named stage before hashing so the
            // report records drift. The miss path logs the discovered
            // artifact set so a silent "found no matching stage" in CI
            // is debuggable from logs alone.
            if let Some(stage) = self.inject_drift.as_deref() {
                match pick_first_artifact_for_stage(&artifacts, stage) {
                    Some(victim) => {
                        inject_drift_byte(victim).with_context(|| {
                            format!(
                                "injecting drift byte into {} on run {}",
                                victim.display(),
                                run_idx
                            )
                        })?;
                    }
                    None => {
                        let summary: Vec<String> = artifacts
                            .iter()
                            .map(|p| {
                                let s = p.to_string_lossy();
                                format!(
                                    "  {} → {}",
                                    p.display(),
                                    artifacts::infer_stage_from_path(&s)
                                )
                            })
                            .collect();
                        StageLogger::new("check-determinism", self.verbosity).warn(&format!(
                            "--inject-drift={} matched no artifact on run {}; \
                             discovered artifacts ({}):\n{}",
                            stage,
                            run_idx,
                            artifacts.len(),
                            summary.join("\n")
                        ));
                    }
                }
            }
            per_run_hashes.push(hash_artifacts(worktree.path(), &artifacts)?);
            // Copy every artifact to a per-run dump directory under the
            // report's parent. This is the diagnostic escape hatch:
            // when drift is detected, the full binaries are uploaded
            // alongside the JSON report so root-causing residual
            // non-determinism doesn't depend on re-running the harness.
            // Non-drifted entries are pruned after the comparison
            // below so the artifact zip stays compact.
            if let Some(parent) = self.report_path.parent() {
                let dump_root = parent.join("drift-bins").join(format!("run-{}", run_idx));
                copy_artifacts_to_dump(worktree.path(), &artifacts, &dump_root, &log)
                    .with_context(|| {
                        format!(
                            "dumping artifacts to {} for determinism run {}",
                            dump_root.display(),
                            run_idx
                        )
                    })?;
            }
            // Preserve run-0's dist tree to the operator-supplied path
            // BEFORE the next iteration's `remove_dir_all` (or this
            // iteration's `Worktree::drop`) wipes it. run-0 is the
            // earliest deterministic pick — runs 1..N are byte-identical
            // to run-0 once the harness passes, but the next run's
            // `remove_dir_all` at the top of the loop deletes the
            // worktree wholesale, so we copy from run-0 specifically.
            //
            // The drift gate happens POST-loop: if drift is detected
            // after all runs finish, we delete the preserved dir below
            // so shippable bytes never escape a failed determinism run.
            if run_idx == 0
                && let Some(dest) = effective_preserve_dest.as_ref()
            {
                preserve_dist_tree(worktree.path(), dest).with_context(|| {
                    format!(
                        "preserving run-0 dist tree from {} to {}",
                        worktree.path().join("dist").display(),
                        dest.display()
                    )
                })?;
                // Mirror raw cargo binaries under `<dest>/bin/<triple>/`
                // and rewrite their paths in `<dest>/artifacts.json` so
                // publish-only's `SignStage` can resolve them under the
                // preserved tree (binaries live outside `dist/` in the
                // worktree and are otherwise lost when the worktree is
                // dropped).
                preserve_raw_binaries(worktree.path(), dest, &log).with_context(|| {
                    format!(
                        "preserving raw binaries from {} into {}",
                        worktree.path().display(),
                        dest.display()
                    )
                })?;
                // No `preserved_dist_filled` flag needed: any error in
                // preserve_dist_tree propagates via `?` and aborts the
                // harness before the post-loop block runs. Reaching
                // post-loop with `self.preserve_dist == Some(_)` is
                // sufficient proof the copy succeeded.
            }
            // Inter-run reclamation: explicitly drop the worktree NOW
            // (rather than at the `}` below) so its entire tree —
            // `.det-tmp/target/**` (the per-run CARGO_TARGET_DIR, the
            // heavy scratch), `.det-tmp/home`, `dist/**`, and the raw
            // per-triple binaries — is freed by `Worktree::drop`'s
            // `git worktree remove --force` BEFORE the next iteration's
            // free-space probe and headroom guard run. Everything the
            // next run consumes is rebuilt from the detached commit, so
            // none of it is read across runs.
            //
            // Determinism-safe: by this point run-0's hashes are already
            // recorded (`per_run_hashes.push` above), the drift-bins dump
            // is already copied out, and — when `--preserve-dist` is set —
            // run-0's dist tree AND raw binaries are already mirrored to
            // `dest`. Nothing freed here feeds the byte comparison or the
            // preserved dist; the worktree is pure rebuild scratch. The
            // drift-bins dump under `<report>/drift-bins/run-N` is
            // deliberately NOT freed here — it is the drift diagnostic and
            // is pruned post-loop only after the comparison decides which
            // runs drifted.
            drop(worktree);
            if let Some(after) = anodizer_core::disk::available_bytes(&worktree_root) {
                log.verbose(&format!(
                    "disk free {}: {} after run {} (worktree reclaimed)",
                    worktree_root.display(),
                    anodizer_core::disk::format_gib(after),
                    run_idx + 1
                ));
            }
        }

        // Best-effort reclaim of the shared CARGO_HOME. It lives OUTSIDE the
        // worktree, so the per-run `Worktree::drop` never touches it; remove it
        // now that all runs are done. It's a throwaway cache (the next
        // invocation re-prefetches into its own pid-suffixed dir), so a leftover
        // on the error path is harmless.
        let _ = std::fs::remove_dir_all(&shared_cargo_home);

        let report = self.build_report(per_run_hashes);
        if let Some(parent) = self.report_path.parent() {
            prune_dump_to_drifted(&parent.join("drift-bins"), &report);
        }
        // Preserve-dist gate. Restructured per code review: if any
        // copy failed mid-loop the `?` propagation already aborted the
        // harness, so reaching this point with
        // `self.preserve_dist == Some(_)` means run-0's tree IS on
        // disk under `dest`. Branch on drift_count alone.
        //
        // Safety property: shippable bytes must come from a green
        // determinism run, never a drifted one. Drift → remove the
        // tree; green → write `<dest>/context.json` so the publish-
        // only path can rehydrate.
        if let Some(dest) = effective_preserve_dest.as_ref() {
            if report.drift_count > 0 {
                remove_preserved_on_drift(dest, &log);
            } else {
                write_preserved_dist_context(
                    dest,
                    ContextInputs {
                        report: &report,
                        harness_targets: self.targets.as_deref(),
                        version_hint: &self.version_hint,
                    },
                    &log,
                )
                .with_context(|| {
                    format!(
                        "writing context.json under preserved dist {}",
                        dest.display()
                    )
                })?;
            }
        }
        Ok(report)
    }

    /// Emit a verbose disk-headroom line for the worktree volume and apply
    /// the fail-fast guard before a determinism run starts.
    ///
    /// `vol` is the worktree-root path (its parent volume backs the
    /// per-run worktree); `free` is the available bytes already probed on
    /// it. `prior_peak` is the largest MEASURED peak consumption of any
    /// prior run (`None` before run-0, when only the absolute floor gates;
    /// `None` thereafter only if the probe was unavailable).
    ///
    /// Routine readings go to `verbose` (per the log-status-vs-verbose
    /// rule); a shortfall is the one default-visible disk event — surfaced
    /// as an `error` line and then returned as an `Err` that aborts the
    /// harness BEFORE the opaque `hdiutil` ENOSPC can fire. Probe gaps
    /// (`free == None`) degrade to a no-op: the guard never manufactures a
    /// failure from missing data (the one-shot warn at the call site
    /// records that the guard is disabled for the invocation).
    pub(crate) fn guard_run_headroom(
        &self,
        log: &StageLogger,
        run_idx: u32,
        vol: &Path,
        free: Option<u64>,
        prior_peak: Option<u64>,
    ) -> Result<()> {
        use anodizer_core::disk::{HeadroomDecision, evaluate_headroom, format_gib};
        let Some(free) = free else {
            return Ok(());
        };
        let vols = anodizer_core::disk::mounted_volumes();
        let mounts = if vols.is_empty() {
            String::new()
        } else {
            format!(" — /Volumes: [{}]", vols.join(", "))
        };
        log.verbose(&format!(
            "disk free {}: {} before run {}{}",
            vol.display(),
            format_gib(free),
            run_idx + 1,
            mounts
        ));
        match evaluate_headroom(
            run_idx,
            free,
            self.disk_abs_floor_bytes,
            prior_peak,
            self.disk_safety_factor,
            &vol.display().to_string(),
        ) {
            HeadroomDecision::Proceed => Ok(()),
            HeadroomDecision::Abort(shortfall) => {
                let msg = shortfall.message();
                log.error(&msg);
                anyhow::bail!(msg)
            }
        }
    }

    /// Construct the env map handed to each child build process.
    pub(crate) fn build_isolated_env(
        &self,
        worktree: &Worktree,
        cargo_home: &Path,
        signing_keys: Option<&EphemeralSigningKeys>,
    ) -> Result<HashMap<String, String>> {
        let tmpdir = worktree.path().join(".det-tmp");
        std::fs::create_dir_all(&tmpdir)?;
        // `cargo_home` is the invocation-wide shared cache (created once in
        // `run()` and warmed by the run-0 prefetch); only the compiled-output
        // dir is per-run-fresh inside the worktree.
        let cargo_target = tmpdir.join("target");
        let home_dir = tmpdir.join("home");
        std::fs::create_dir_all(&home_dir)?;

        Ok(build_subprocess_env(&BuildSubprocessEnv {
            cargo_home,
            cargo_target: &cargo_target,
            tmpdir: &tmpdir,
            home_dir: &home_dir,
            sde: self.sde,
            worktree: worktree.path(),
            targets: self.targets.as_deref().unwrap_or(&[]),
            signing_keys,
        }))
    }

    /// Shell to the running `anodize` binary inside the worktree.
    ///
    /// Delegates to [`anodizer_core::determinism_runner`] — `crates/cli/**`
    /// is on the forbid-list for direct subprocess spawn, so the actual
    /// `Command::new` lives in core where it's allow-listed.
    ///
    /// `effective_stages` is what the harness actually ran the child
    /// pipeline against — usually equal to `self.stages`, but with
    /// `Sign` filtered out when [`Harness::preserve_dist`] is set AND
    /// production signing keys are present on the runner (so the harness
    /// doesn't leave ephemeral sigs in the preserved dist; they would
    /// only get stripped + re-signed later anyway).
    pub(crate) fn run_build_pipeline(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        effective_stages: &[StageId],
    ) -> Result<()> {
        let exe = anodizer_core::determinism_runner::current_anodize_binary()?;
        let extra_skip = compute_extra_skip(effective_stages);
        anodizer_core::determinism_runner::run_build_pipeline_subprocess(
            &anodizer_core::determinism_runner::ChildInvocation {
                anodize_binary: &exe,
                worktree_path,
                env,
                targets: self.targets.as_deref(),
                extra_skip: &extra_skip,
                snapshot: self.child_snapshot,
                crate_name: self.crate_name.as_deref(),
                verbosity: self.verbosity,
            },
        )
    }

    /// Drive the `cargo-package` stage when [`StageId::CargoPackage`] is
    /// in the requested stage set.
    ///
    /// Delegates to [`anodizer_core::cargo_package::package_workspace`]
    /// (the allow-listed subprocess entry point), then copies the
    /// emitted `<cargo_target>/package/*.crate` into
    /// `<worktree>/dist/cargo-package/` so the existing
    /// [`discover_artifacts`] walker picks them up under the normal
    /// `dist/` surface.
    ///
    /// `SOURCE_DATE_EPOCH` is already in `env` — the harness exports it
    /// from [`Harness::sde`] via [`super::env::build_subprocess_env`].
    /// cargo (≥ 1.74) canonicalizes mtimes inside the `.crate` tar to
    /// the supplied epoch and sorts tar members alphabetically, which
    /// covers the two leading non-determinism sources. Residual drift
    /// (`.cargo_vcs_info.json` contents, registry path embedding,
    /// future cargo regressions) will appear in the report's `drift`
    /// section instead of silently passing.
    pub(crate) fn run_cargo_package(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let log = StageLogger::new("check-determinism", self.verbosity);
        anodizer_core::cargo_package::package_workspace(worktree_path, env, &log)?;
        // cargo writes to `<cargo_target>/package/<name>-<version>.crate`
        // where `cargo_target` came from `CARGO_TARGET_DIR` in the env
        // block. The env block sets `CARGO_TARGET_DIR=<worktree>/.det-tmp/target`
        // so the .crate files land there.
        let source = worktree_path
            .join(".det-tmp")
            .join("target")
            .join("package");
        let dest = worktree_path.join("dist").join("cargo-package");
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating dest dir {}", dest.display()))?;
        if !source.exists() {
            // No `.crate` files emitted (e.g. workspace virtual manifest
            // with no `[package]` member). Treat as a no-op so the harness
            // doesn't fail when an operator points it at a virtual
            // workspace by mistake — the resulting drift report will be
            // empty for the cargo-package stage, which correctly reflects
            // "nothing exercised".
            return Ok(());
        }
        for entry in
            std::fs::read_dir(&source).with_context(|| format!("reading {}", source.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("crate") {
                let name = path
                    .file_name()
                    .with_context(|| format!("crate path lacks filename: {}", path.display()))?;
                let target = dest.join(name);
                std::fs::copy(&path, &target).with_context(|| {
                    format!("copying {} → {}", path.display(), target.display())
                })?;
            }
        }
        Ok(())
    }
}
