use super::*;

impl Harness {
    /// Drive the `docker` stage when [`StageId::Docker`] is in the
    /// requested stage set.
    ///
    /// Iterates the crate-under-test's resolved [`ResolvedDockerConfig`]
    /// entries ([`Self::docker_configs`]) — every configured `dockers_v2`
    /// image, not just one. For each, it stages a dedicated context (see
    /// [`stage_docker_context`]: per-triple binaries at
    /// `<context>/<os>/<arch>/<bin>`, the config's rendered dockerfile copied
    /// to `<context>/Dockerfile`, and the config's `extra_files`) then
    /// delegates to [`anodizer_core::docker_build::oci_build_fixture`] (the
    /// allow-listed subprocess entry point), which runs
    /// `docker buildx build --output=type=oci,rewrite-timestamp=true,dest=…`
    /// with the config's `build_args`. Each emitted OCI tarball is copied to
    /// `<worktree>/dist/docker/<idx>/image.oci.tar` (per-config subdir so
    /// multiple images don't collide) where the existing [`discover_artifacts`]
    /// walker picks it up under the normal `dist/` surface.
    ///
    /// The configured dockerfile is the SAME one the production `docker` stage
    /// builds — never a hardcoded repo-root `Dockerfile`. A project with a thin
    /// release dockerfile (`COPY ${TARGETOS}/${TARGETARCH}/${BIN}`) distinct
    /// from a fat dev `Dockerfile` at the repo root gets its release image
    /// byte-verified, not the dev one against an incomplete context.
    ///
    /// Skipped (Ok no-op) when [`Self::docker_configs`] is empty — the crate
    /// configures no `dockers_v2`, so there is nothing to byte-compare. This
    /// skip is unconditional (never coverage loss regardless of intent) and
    /// keeps the stage harmless for cargo-package / cargo-only fixtures that
    /// share the harness binary.
    ///
    /// When `docker buildx` is unreachable or the project opted into
    /// `use: podman`, the behaviour forks on `explicitly_requested`:
    /// - `true` (operator typed `--stages=…,docker`): a hard ERROR. A
    ///   determinism gate that silently skips a stage the caller asked it
    ///   to byte-verify is false coverage — a non-reproducible image could
    ///   ship while the gate reports green. The release pipeline's ubuntu
    ///   shard requests docker explicitly and provisions a
    ///   `docker-container` buildx driver, so this error fires only when
    ///   that provisioning regressed.
    /// - `false` (host-default, not operator-typed): a warning through the
    ///   harness logger (so `-q` silences it). `docker` IS in the Linux host
    ///   default, so a bare `anodize check determinism` on a Linux box without
    ///   `docker buildx` reaches this branch and warn-skips rather than failing
    ///   the whole harness — the harness also runs on minimal images (e.g. the
    ///   docs build container) that legitimately lack Docker, where failing
    ///   would block unrelated stages.
    pub(crate) fn run_docker_stage(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        explicitly_requested: bool,
    ) -> Result<()> {
        let log = StageLogger::new("check-determinism", self.verbosity);
        // Fork on the two empty-`docker_configs` cases. Resolution ERRORS are
        // already hard failures upstream (`resolve_docker_configs` propagates
        // every skip-eval / dockerfile / build_arg render error via `?`), so an
        // empty set here is NEVER a swallowed error — only:
        // - crate declares no `dockers_v2` → nothing to byte-verify → clean
        //   skip (and never a stray repo-root `Dockerfile`); or
        // - crate DECLARES `dockers_v2` but every entry was LEGITIMATELY
        //   skipped in this context (truthy `skip:` — e.g. the common
        //   `skip: "{{ .IsSnapshot }}"` under the harness's snapshot mode — or
        //   an empty-rendered conditional dockerfile).
        //
        // The all-skipped case must MIRROR production, which cleanly skips
        // (DockerStage::run leaves `build_jobs` empty → no build, no error;
        // prepare_v2_config `return Ok(())`s a skipped / empty-render entry).
        // Hard-failing it under `--require-tools` would be a false FAILURE —
        // the mirror of the false pass — turning every determinism run of a
        // `skip-on-snapshot` config red. So warn-and-skip even when explicitly
        // requested; the warn keeps it from being silent.
        if self.docker_configs.is_empty() {
            if self.docker_declared {
                log.warn(
                    "skipped docker stage for this run — crate declares dockers_v2 but all \
                     entries are skipped in this context (`skip:` / empty-rendered dockerfile); \
                     docker byte-verification produced no image (matches a normal release)",
                );
            }
            return Ok(());
        }
        // The determinism harness's docker probe shells out to
        // `docker buildx build --output=type=oci,rewrite-timestamp=true,...`.
        // Those BuildKit-only flags are not recognised by plain
        // `podman build`; when the project config opts into `use: podman`
        // the only honest behaviour is to skip the docker stage with a
        // clear message, rather than spawn `docker buildx` and hand the
        // operator a misleading "this image is reproducible" signal that
        // covers a binary they will never actually publish.
        // These warnings go through the harness logger so `-q` governs
        // them like every other harness line; the docker-buildx child's
        // own output below is captured by `run_checked` and surfaced only
        // at `-v` (or on failure), so it honours the same verbosity flag.
        if self.docker_backend_hint.as_deref() == Some("podman") {
            let msg = "docker stage requested but project config has `use: podman` \
                 (Linux-only); the determinism harness only probes BuildKit-based \
                 builds. Verify podman image byte-stability outside the harness.";
            if explicitly_requested {
                anyhow::bail!(
                    "{msg} Refusing to report byte-stability for an image the harness \
                     cannot probe — remove `docker` from --stages or build the image \
                     with BuildKit."
                );
            }
            log.warn(&format!("{msg} The docker stage is skipped for this run."));
            return Ok(());
        }
        match anodizer_core::docker_detect::buildx_available() {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                if explicitly_requested {
                    anyhow::bail!(
                        "docker stage requested via --stages but `docker buildx` is not \
                         available on PATH; the determinism gate cannot byte-verify the \
                         image. Provision a `docker-container` buildx driver \
                         (docker/setup-buildx-action) before running the harness."
                    );
                }
                log.warn(
                    "skipped docker stage for this run — `docker buildx` is not available on PATH \
                     (no artifacts emitted)",
                );
                return Ok(());
            }
        }
        // Build EVERY configured image so the gate covers each, not just one.
        for (idx, docker_cfg) in self.docker_configs.iter().enumerate() {
            self.run_one_docker_config(
                worktree_path,
                env,
                &log,
                idx,
                docker_cfg,
                explicitly_requested,
            )?;
        }
        Ok(())
    }

    /// Stage + build one resolved `dockers_v2` entry for the current run,
    /// emitting its OCI tarball (and BuildKit digest) under
    /// `<worktree>/dist/docker/<idx>/`.
    pub(crate) fn run_one_docker_config(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        log: &StageLogger,
        idx: usize,
        docker_cfg: &ResolvedDockerConfig,
        explicitly_requested: bool,
    ) -> Result<()> {
        // A configured dockerfile that isn't in the committed worktree would
        // also fail the production release (the release rebuilds the same
        // commit). Fork on intent like the buildx / staged-binary gates: an
        // explicit request hard-fails (silent skip = false coverage); a
        // host-default run warn-skips so a dev box stays usable.
        let dockerfile_abs = worktree_path.join(&docker_cfg.dockerfile);
        if !dockerfile_abs.exists() {
            if explicitly_requested {
                anyhow::bail!(
                    "dockers_v2[{idx}] dockerfile '{}' does not exist in the rebuilt \
                     worktree; the determinism docker stage cannot byte-verify it. \
                     Ensure the dockerfile is committed.",
                    docker_cfg.dockerfile
                );
            }
            log.warn(&format!(
                "skipped dockers_v2[{idx}] for this run — dockerfile '{}' not found in the \
                 rebuilt worktree (no artifacts emitted)",
                docker_cfg.dockerfile
            ));
            return Ok(());
        }
        // Per-config context dir so multiple images don't clobber each other.
        let context_dir = worktree_path
            .join(".det-tmp")
            .join(format!("docker-context-{idx}"));
        let staged = stage_docker_context(worktree_path, &context_dir, docker_cfg, log)?;
        if staged == 0 {
            // No per-triple binaries discovered means the build pipeline
            // produced nothing the dockerfile's COPY could resolve. Honour
            // the explicit-vs-auto fork rather than spawn a build that is
            // guaranteed to fail the COPY with a cryptic BuildKit error.
            if explicitly_requested {
                anyhow::bail!(
                    "docker stage requested via --stages but the build produced no \
                     per-triple binaries to stage under <os>/<arch>/; the COPY in \
                     dockers_v2[{idx}]'s dockerfile cannot resolve. Check that the \
                     requested --targets built successfully."
                );
            }
            log.warn(
                "skipped docker stage for this run — no per-triple binaries to stage \
                 under <os>/<arch>/ (no artifacts emitted)",
            );
            return Ok(());
        }
        // Pin the image tag to a deterministic per-config constant so the
        // manifest's `org.opencontainers.image.ref.name` annotation does not
        // itself drift between runs based on time-derived names.
        let output = anodizer_core::docker_build::oci_build_fixture(
            &context_dir,
            &format!("anodize/det:harness-{idx}"),
            &docker_cfg.build_args,
            env,
            log,
        )?;
        let dest_dir = worktree_path
            .join("dist")
            .join("docker")
            .join(idx.to_string());
        std::fs::create_dir_all(&dest_dir)
            .with_context(|| format!("creating dest dir {}", dest_dir.display()))?;
        // Rename to a stable filename so the artifact-discovery walker
        // surfaces a single canonical row regardless of where buildx
        // emitted the tarball under the worktree.
        let target = dest_dir.join("image.oci.tar");
        std::fs::copy(&output.oci_tar_path, &target).with_context(|| {
            format!(
                "copying {} → {}",
                output.oci_tar_path.display(),
                target.display()
            )
        })?;
        // Capture the BuildKit-reported image digest alongside the OCI
        // tarball so the report records it as a separately-diffed
        // artifact. The two are independent stability signals: the
        // tarball hash covers serialized bytes (layer tar member
        // ordering, manifest serialization), while the iidfile records
        // BuildKit's pre-serialization manifest digest. Both must be
        // stable for the image to be declared byte-stable.
        if let Some(digest) = output.image_digest.as_deref() {
            std::fs::write(dest_dir.join("image.digest"), digest).with_context(|| {
                format!(
                    "writing image digest to {}",
                    dest_dir.join("image.digest").display()
                )
            })?;
        }
        Ok(())
    }
}
