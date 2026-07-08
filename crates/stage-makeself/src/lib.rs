use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// LSM (Linux Software Map) metadata
// ---------------------------------------------------------------------------

struct Lsm {
    title: String,
    version: String,
    description: String,
    keywords: Vec<String>,
    maintained_by: String,
    primary_site: String,
    platform: String,
    copying_policy: String,
}

impl Lsm {
    fn render(&self) -> String {
        let mut sb = String::from("Begin4\n");
        let mut w = |name: &str, value: &str| {
            if !value.is_empty() {
                sb.push_str(&format!("{}: {}\n", name, value));
            }
        };
        w("Title", &self.title);
        w("Version", &self.version);
        w("Description", &self.description);
        if !self.keywords.is_empty() {
            w("Keywords", &self.keywords.join(", "));
        }
        w("Maintained-by", &self.maintained_by);
        w("Author", &self.maintained_by);
        w("Primary-site", &self.primary_site);
        w("Platforms", &self.platform);
        w("Copying-policy", &self.copying_policy);
        sb.push_str("End");
        sb
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default `.run` filename template when a config sets no `filename:`.
///
/// `makeself` is a Linux-capable namer, so it appends the FULL
/// [`MICRO_ARCH_VARIANT_SUFFIX`](anodizer_core::archive_name::MICRO_ARCH_VARIANT_SUFFIX)
/// (Arm/Mips/Amd64) — the same suffix the archive defaults carry — rather than
/// the amd64-only installer subset, composed from the shared const so it cannot
/// drift from the archive stage's naming.
fn default_name_template() -> String {
    format!(
        "{{{{ ProjectName }}}}_{{{{ Version }}}}_{{{{ Os }}}}_{{{{ Arch }}}}{}.run",
        anodizer_core::archive_name::MICRO_ARCH_VARIANT_SUFFIX
    )
}

/// Build the makeself command arguments.
fn make_args(
    name: &str,
    filename: &str,
    compression: Option<&str>,
    script: &str,
    extra_args: &[String],
    packaging_date: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["--quiet".to_string()];

    // Compression default is `--xz` (not makeself's `--gzip`): the gzip
    // pipeline embeds the wallclock mtime of the intermediate tarball into
    // the gzip stream header (gzip reads stdin from a regular file, sees its
    // mtime, ignores SOURCE_DATE_EPOCH), which drifts the compressed bytes
    // and the CRCsum/MD5/SHA values embedded in the .run shell header. xz
    // does not embed wall-clock data and is byte-stable under SDE. Users can
    // override via `compression: gzip` in config but should expect non-
    // deterministic .run output under the harness in that case.
    match compression {
        Some("gzip") => args.push("--gzip".to_string()),
        Some("bzip2") => args.push("--bzip2".to_string()),
        Some("xz") => args.push("--xz".to_string()),
        Some("lzo") => args.push("--lzo".to_string()),
        Some("compress") => args.push("--compress".to_string()),
        Some("none") => args.push("--nocomp".to_string()),
        None => args.push("--xz".to_string()),
        Some(_) => {} // unknown values pass through to makeself default
    }

    args.push("--lsm".to_string());
    args.push("package.lsm".to_string());

    // Pin the extraction-target dir name. Without `--target`, makeself falls
    // back to `makeself-$$-$(date +%Y%m%d%H%M%S)` which embeds the PID + the
    // wallclock and drifts byte-by-byte between two harness runs. The
    // filename (sans `.run` extension) is stable across runs by construction
    // (it's templated from config + version + target) and reads naturally
    // when the user extracts to inspect the archive.
    let target_dir = filename.strip_suffix(".run").unwrap_or(filename);
    args.push("--target".to_string());
    args.push(target_dir.to_string());

    // Pin the packaging-date header under SOURCE_DATE_EPOCH so the .run
    // header is byte-stable across runs. makeself's default
    // `DATE=`LC_ALL=C date`` reads wall-clock and otherwise leaks into the
    // .run file (the `Date of packaging:` line in the embedded header).
    if let Some(date) = packaging_date {
        args.push("--packaging-date".to_string());
        args.push(date.to_string());
    }

    args.extend(extra_args.iter().cloned());

    // positional args: archive_dir output_file label startup_script.
    // Output is the bare filename (relative to current_dir); using an
    // absolute path here would leak the per-run worktree prefix into
    // makeself's embedded `MS_COMMAND` variable and break determinism.
    args.push(".".to_string());
    args.push(filename.to_string());
    args.push(name.to_string());
    args.push(script.to_string());

    args
}

/// Format a packaging date string for makeself's `--packaging-date` flag,
/// reading `SOURCE_DATE_EPOCH` from the injected env source.
///
/// Returns `None` when SDE is unset so normal production runs keep
/// makeself's default `LC_ALL=C date` behaviour. The injectable form
/// keeps tests off process-env mutation.
fn resolve_packaging_date_with_env<E: anodizer_core::env_source::EnvSource + ?Sized>(
    env: &E,
) -> Option<String> {
    // Format mirrors `LC_ALL=C date -u` output (which is what makeself's
    // default `DATE=`LC_ALL=C date`` produces in the harness's UTC=Etc/UTC
    // env), keeping the embedded `Date of packaging:` line readable.
    anodizer_core::sde::source_date_epoch_with_env(env)
        .map(|dt| dt.format("%a %b %e %H:%M:%S UTC %Y").to_string())
}

/// Process-env convenience wrapper over [`resolve_packaging_date_with_env`].
fn resolve_packaging_date() -> Option<String> {
    resolve_packaging_date_with_env(&anodizer_core::env_source::ProcessEnvSource)
}

/// Recursively pin every regular file's mtime in `dir` to `epoch_secs`.
///
/// makeself wraps `tar` over the work directory; tar embeds each file's
/// on-disk mtime into the archive header. `fs::copy` stamps destination
/// files with the current wallclock, so two harness runs produce identical
/// file contents but with different mtimes — that drifts the tar payload,
/// the gzip compressed bytes (length + content), and ultimately the
/// `size_bytes` field that artifacts.json carries for the `.run` artifact.
/// Pinning every file's mtime to `SOURCE_DATE_EPOCH` removes the drift.
fn pin_workdir_mtimes(dir: &Path, epoch_secs: i64) -> Result<()> {
    anodizer_core::util::pin_dir_mtimes_epoch(dir, epoch_secs)
        .with_context(|| format!("makeself: pin staging mtimes under {}", dir.display()))
}

/// Reject duplicate makeself config IDs (per-id `default` collapses unkeyed
/// entries onto one slot — same shape as nfpm/snapcraft validation).
fn validate_unique_ids(configs: &[anodizer_core::config::MakeselfConfig]) -> Result<()> {
    let mut seen_ids = std::collections::HashSet::new();
    for cfg in configs {
        let id = cfg.id.as_deref().unwrap_or("default");
        if !seen_ids.insert(id.to_string()) {
            anyhow::bail!("makeself: duplicate id '{}'", id);
        }
    }
    Ok(())
}

/// Filter and clone the binary-like artifacts that match a makeself config's
/// id-filter + os/arch selectors. The result is owned so the surrounding
/// loop can drop its borrow on `ctx.artifacts` before re-borrowing `ctx` for
/// template rendering.
fn collect_matching_binaries(
    ctx: &Context,
    cfg: &anodizer_core::config::MakeselfConfig,
    os_filter: &[String],
) -> Vec<Artifact> {
    ctx.artifacts
        .all()
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                ArtifactKind::Binary
                    | ArtifactKind::UniversalBinary
                    | ArtifactKind::Header
                    | ArtifactKind::CArchive
                    | ArtifactKind::CShared
            )
        })
        .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
        .filter(|a| {
            if let Some(ref target) = a.target {
                let (os, _) = anodizer_core::target::map_target(target);
                os_filter.iter().any(|o| o == &os)
            } else {
                false
            }
        })
        .filter(|a| {
            if let Some(ref arch_filter) = cfg.arch {
                if let Some(ref target) = a.target {
                    let (_, arch) = anodizer_core::target::map_target(target);
                    arch_filter.iter().any(|a| a == &arch)
                } else {
                    false
                }
            } else {
                true
            }
        })
        .cloned()
        .collect()
}

/// Seed Os / Arch / Target plus the per-target variant template vars (Arm,
/// Arm64, Amd64, Mips, I386) so the default name_template renders correctly.
///
/// The variant vars come from the shared
/// [`seed_variant_vars`](anodizer_core::archive_name::seed_variant_vars)
/// policy — the same seeding the build stage applies to binary-name templates,
/// so a user template's `{{ .Amd64 }}` renders identically in both places.
/// `amd64_variant` is the built binary's `amd64_variant` metadata: it replaces
/// the `"v1"` baseline so two amd64 builds of one target (a baseline and a
/// `-Ctarget-cpu=x86-64-v3` tune) render distinct `.run` names; the default
/// template's `!= "v1"` guard keeps the baseline suffix-free.
fn set_per_target_template_vars(
    ctx: &mut Context,
    target: Option<&str>,
    os: &str,
    arch: &str,
    amd64_variant: Option<&str>,
) {
    ctx.template_vars_mut().set("Os", os);
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    anodizer_core::archive_name::seed_variant_vars(
        ctx.template_vars_mut(),
        target.unwrap_or(""),
        amd64_variant,
    );
}

/// Render the makeself output filename for a single (target, platform) combo.
///
/// Honors `cfg.filename` as a Tera template when set; falls back to a
/// project/version/os/arch composite that includes the per-arch variant
/// suffix so multi-target ARM / MIPS / x86 builds for the same project
/// don't collide on disk.
fn resolve_makeself_filename(
    ctx: &Context,
    name_template: &str,
    project_name: &str,
    version: &str,
    os: &str,
    arch: &str,
) -> Result<String> {
    if !name_template.is_empty() {
        let rendered = ctx.render_template(name_template)?;
        return Ok(if rendered.ends_with(".run") {
            rendered
        } else {
            format!("{}.run", rendered)
        });
    }
    let arm = ctx.template_vars().get("Arm").cloned().unwrap_or_default();
    let mips = ctx.template_vars().get("Mips").cloned().unwrap_or_default();
    let amd64 = ctx
        .template_vars()
        .get("Amd64")
        .cloned()
        .unwrap_or_default();
    let mut suffix = String::new();
    if !arm.is_empty() {
        suffix.push('v');
        suffix.push_str(&arm);
    }
    if !mips.is_empty() {
        suffix.push('_');
        suffix.push_str(&mips);
    }
    if !amd64.is_empty() && amd64 != "v1" {
        suffix.push_str(&amd64);
    }
    Ok(format!(
        "{}_{}_{}_{}{}.run",
        project_name, version, os, arch, suffix
    ))
}

/// Execute a fully-prepared makeself job: stage files into the work_dir,
/// pin mtimes for SDE determinism, invoke `makeself`, move the output into
/// place, and produce the registered `Artifact`.
///
/// Thread-safe: borrows only the owned data on `MakeselfJob`; never touches
/// `Context`. Returns the artifact for serial registration by the caller.
fn execute_makeself_job(
    job: &MakeselfJob,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Artifact> {
    let thread_log = anodizer_core::log::StageLogger::new("makeself", verbosity);

    fs::create_dir_all(&job.work_dir)
        .with_context(|| format!("makeself: create dir {}", job.work_dir.display()))?;

    for (src, name) in &job.binaries {
        let dst = job.work_dir.join(name);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dst).with_context(|| {
            format!(
                "makeself: copy binary {} → {}",
                src.display(),
                dst.display()
            )
        })?;
    }

    for (src, dest_rel) in &job.extra_files {
        let dst = job.work_dir.join(dest_rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dst).with_context(|| {
            format!("makeself: copy file {} → {}", src.display(), dst.display())
        })?;
    }

    fs::copy(&job.script_src, job.work_dir.join(&job.script_basename))
        .with_context(|| format!("makeself: copy script {}", job.script_src.display()))?;

    fs::write(job.work_dir.join("package.lsm"), &job.lsm_text)
        .with_context(|| format!("makeself: write LSM file in {}", job.work_dir.display()))?;

    // Pin every file's mtime to SOURCE_DATE_EPOCH so tar embeds the same
    // per-file timestamps across runs. Without this, fs::copy stamps the
    // destination with the current wallclock and the resulting tar.gz
    // payload differs between consecutive runs.
    if let Some(epoch) = anodizer_core::sde::source_date_epoch().map(|dt| dt.timestamp()) {
        pin_workdir_mtimes(&job.work_dir, epoch)?;
    }

    let packaging_date = resolve_packaging_date();
    let args = make_args(
        &job.rendered_name,
        &job.filename,
        job.rendered_compression.as_deref(),
        &format!("./{}", job.script_basename),
        &job.extra_args,
        packaging_date.as_deref(),
    );

    thread_log.status(&format!("creating makeself package {}", job.filename));

    let mut command = Command::new("makeself");
    command.args(&args).current_dir(&job.work_dir);
    anodizer_core::run::run_checked(
        &mut command,
        &thread_log,
        &format!("makeself '{}' (id={})", job.filename, job.id),
    )?;

    let built_path = job.work_dir.join(&job.filename);
    fs::rename(&built_path, &job.output_path)
        .or_else(|_| {
            fs::copy(&built_path, &job.output_path)?;
            fs::remove_file(&built_path)
        })
        .with_context(|| {
            format!(
                "makeself: move {} → {}",
                built_path.display(),
                job.output_path.display()
            )
        })?;

    Ok(Artifact {
        kind: ArtifactKind::Makeself,
        name: job.filename.clone(),
        path: job.output_path.clone(),
        target: job.primary_target.clone(),
        crate_name: job.primary_crate_name.clone(),
        metadata: makeself_artifact_metadata(job),
        size: None,
    })
}

/// Build the metadata map stamped onto a produced `.run` artifact: the `id`,
/// the `format` tag, any `replaces` declaration, and — for an amd64 build
/// tuned past baseline — the `amd64_variant`, so downstream stages can tell two
/// amd64 builds of one target apart. A baseline (`None` / `v1`) records no
/// variant, preserving the historical metadata shape.
fn makeself_artifact_metadata(job: &MakeselfJob) -> HashMap<String, String> {
    let mut metadata = HashMap::new();
    metadata.insert("id".to_string(), job.id.clone());
    metadata.insert("format".to_string(), "makeself".to_string());
    if let Some(replaces) = &job.primary_replaces {
        metadata.insert("replaces".to_string(), replaces.clone());
    }
    if let Some(variant) = &job.amd64_variant {
        metadata.insert("amd64_variant".to_string(), variant.clone());
    }
    metadata
}

/// Group artifacts by `(platform, amd64_variant)` — e.g.
/// `("linux_amd64", Some("v3"))`.
///
/// The key carries the binary's `amd64_variant` metadata alongside the os/arch
/// platform string so two amd64 builds of one target (a baseline `v1` and a
/// `-Ctarget-cpu=x86-64-v3` tune) land in separate groups and produce two
/// distinct `.run` files instead of one silently clobbering the other.
///
/// `BTreeMap` (not `HashMap`) so iteration order is deterministic across
/// runs — callers iterate the result to register one makeself Artifact per
/// group, and `HashMap` iteration order is randomised per process. The
/// matching `stage-archive` regression shipped per-run drift into
/// `dist/artifacts.json`; this stage uses the same pattern so it gets the
/// same fix preemptively.
fn group_by_platform(artifacts: &[Artifact]) -> BTreeMap<(String, Option<String>), Vec<&Artifact>> {
    let mut groups: BTreeMap<(String, Option<String>), Vec<&Artifact>> = BTreeMap::new();
    for a in artifacts {
        let platform = match &a.target {
            Some(t) => {
                let (os, arch) = anodizer_core::target::map_target(t);
                format!("{}_{}", os, arch)
            }
            None => "unknown".to_string(),
        };
        let variant = a.metadata.get("amd64_variant").cloned();
        groups.entry((platform, variant)).or_default().push(a);
    }
    groups
}

// ---------------------------------------------------------------------------
// MakeselfStage
// ---------------------------------------------------------------------------

pub struct MakeselfStage;

/// A fully-prepared makeself job ready for parallel execution. The serial
/// phase (requires `&mut ctx` for template rendering) populates this;
/// the parallel phase (`std::thread::scope`) consumes it — filesystem
/// preparation + `makeself` subprocess only. Struct carries only owned data
/// so worker threads never touch `ctx`.
struct MakeselfJob {
    id: String,
    filename: String,
    work_dir: std::path::PathBuf,
    output_path: std::path::PathBuf,
    rendered_name: String,
    script_src: std::path::PathBuf,
    script_basename: String,
    rendered_compression: Option<String>,
    extra_args: Vec<String>,
    lsm_text: String,
    /// (source_path, name_in_archive) pairs for each binary to copy in.
    binaries: Vec<(std::path::PathBuf, String)>,
    /// (source_path, destination_relative_to_work_dir) pairs for extra files.
    extra_files: Vec<(std::path::PathBuf, String)>,
    /// Fields needed to register the resulting artifact.
    primary_target: Option<String>,
    primary_crate_name: String,
    primary_replaces: Option<String>,
    /// The group's amd64 micro-architecture variant (`None` / `Some("v1")`
    /// → baseline), stamped onto the produced `.run` artifact's metadata so
    /// downstream stages can tell two amd64 builds of one target apart.
    amd64_variant: Option<String>,
}

impl Stage for MakeselfStage {
    fn name(&self) -> &str {
        "makeself"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("makeself");
        let configs = ctx.config.makeselfs.clone();

        if configs.is_empty() {
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let parallelism = ctx.options.parallelism.max(1);

        validate_unique_ids(&configs)?;

        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        let project_name = ctx.config.project_name.clone();

        // ----------------------------------------------------------------
        // Serial: render every template, collect MakeselfJob structs
        // containing fully-owned data ready for parallel exec.
        // ----------------------------------------------------------------
        let mut jobs: Vec<MakeselfJob> = Vec::new();

        // One guard spans every `makeselfs:` config of the project: two configs
        // with the default (or identical) `filename:` render the same `.run`
        // path for one platform — error loudly across configs instead of letting
        // the second silently clobber the first.
        let mut arch_guard = ArchPathGuard::new();

        for cfg in &configs {
            collect_makeself_config_jobs(
                ctx,
                &log,
                cfg,
                &dist,
                &version,
                &project_name,
                dry_run,
                &mut arch_guard,
                &mut jobs,
            )?;
        }

        if jobs.is_empty() {
            return Ok(());
        }

        // ----------------------------------------------------------------
        // Parallel: each job = one `makeself` subprocess invocation with
        // its own work dir. Bounded concurrency via chunks(parallelism).
        // Workers return the fully-populated `Artifact` for serial
        // registration in ctx.artifacts below.
        // ----------------------------------------------------------------
        let verbosity = log.verbosity();
        let built_artifacts = anodizer_core::parallel::run_parallel_chunks(
            &jobs,
            parallelism,
            "makeself",
            &log,
            |job: &MakeselfJob| execute_makeself_job(job, verbosity),
        )?;

        // ----------------------------------------------------------------
        // Serial: register artifacts in ctx. ArtifactRegistry takes &mut self.
        // ----------------------------------------------------------------
        for artifact in built_artifacts {
            ctx.artifacts.add(artifact);
        }

        // Match flatpak/snapcraft/nfpm: clear per-target template vars so
        // downstream stages (announce, publish) don't render with a stale
        // `Os=linux` / `Arch=arm64` left over from the last packaging
        // iteration.
        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        Ok(())
    }
}

/// Collect `MakeselfJob` entries for one makeself config: validates, groups
/// binaries by platform, renders all templates, and pushes jobs (or emits
/// dry-run output).
#[allow(clippy::too_many_arguments)]
fn collect_makeself_config_jobs(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    cfg: &anodizer_core::config::MakeselfConfig,
    dist: &std::path::Path,
    version: &str,
    project_name: &str,
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    jobs: &mut Vec<MakeselfJob>,
) -> Result<()> {
    if let Some(ref d) = cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "makeself: render skip template")?;
        if off {
            log.verbose("makeself config skipped");
            return Ok(());
        }
    }

    let id = cfg.id.as_deref().unwrap_or("default");
    let name = cfg.name.as_deref().unwrap_or(project_name);
    let default_name_template = default_name_template();
    let name_template = cfg
        .filename
        .as_deref()
        .unwrap_or(default_name_template.as_str());

    let script = cfg.script.as_deref().unwrap_or("");
    if script.is_empty() {
        anyhow::bail!("makeself: 'script' is required for config id '{}'", id);
    }

    let os_filter: Vec<String> = cfg
        .os
        .clone()
        .unwrap_or_else(|| vec!["linux".to_string(), "darwin".to_string()]);

    let all_binaries = collect_matching_binaries(ctx, cfg, &os_filter);

    if all_binaries.is_empty() {
        // A zero-match is only a real config error in a FULL, unrestricted
        // release: every selected crate's binaries for every target are
        // present, so an empty set means the config's os/arch/ids scope can
        // never match. Under a restricted build the set is incomplete BY
        // CONSTRUCTION, so a config scoped to a crate/target that isn't in
        // this slice legitimately matches nothing and must step aside (mirrors
        // emission-validate / sbom skipping under `--targets`):
        //   - `--targets` (`partial_target`) trims the built targets, so a
        //     darwin-only or different-arch config finds no binary.
        //   - per-crate determinism (`--crate cfgd-csi`, `selected_crates`
        //     non-empty) builds one crate, so a config scoped to a different
        //     crate's binary id (cfgd's `ids: [cfgd]`) finds nothing.
        if ctx.options.partial_target.is_some() || !ctx.options.selected_crates.is_empty() {
            log.status(&format!(
                "skipped makeself config '{}' — no binaries match its os {:?} \
                 under a restricted build (--targets / per-crate); the config's \
                 crate/target scope isn't part of this slice",
                id, os_filter
            ));
            return Ok(());
        }
        anyhow::bail!(
            "makeself: no binaries found for config '{}' with os {:?}",
            id,
            os_filter
        );
    }

    let groups = group_by_platform(&all_binaries);

    for ((platform, amd64_variant), binaries) in &groups {
        build_makeself_platform_job(
            ctx,
            log,
            cfg,
            id,
            name,
            name_template,
            script,
            dist,
            version,
            project_name,
            platform,
            amd64_variant.as_deref(),
            binaries,
            dry_run,
            arch_guard,
            jobs,
        )?;
    }

    Ok(())
}

/// Render templates and build a `MakeselfJob` for one platform within a
/// makeself config, or emit dry-run output.
#[allow(clippy::too_many_arguments)]
fn build_makeself_platform_job(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    cfg: &anodizer_core::config::MakeselfConfig,
    id: &str,
    name: &str,
    name_template: &str,
    script: &str,
    dist: &std::path::Path,
    version: &str,
    project_name: &str,
    platform: &str,
    amd64_variant: Option<&str>,
    binaries: &[&Artifact],
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    jobs: &mut Vec<MakeselfJob>,
) -> Result<()> {
    let primary = binaries[0];
    let (os, arch) = primary
        .target
        .as_deref()
        .map(anodizer_core::target::map_target)
        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

    set_per_target_template_vars(ctx, primary.target.as_deref(), &os, &arch, amd64_variant);

    let rendered_name = if cfg.name.is_some() {
        ctx.render_template(name)?
    } else {
        project_name.to_string()
    };

    let filename =
        resolve_makeself_filename(ctx, name_template, project_name, version, &os, &arch)?;

    // Reject a `filename:` that renders the same `.run` path for two platforms
    // or two amd64 variants (a constant override lacking `{{ .Arch }}` /
    // `{{ .Amd64 }}`): the second job would silently overwrite the first.
    let output_path = dist.join(&filename);
    arch_guard.check(
        &output_path,
        "makeself",
        "package",
        name_template,
        &filename,
        &primary.crate_name,
    )?;

    let rendered_description = cfg
        .description
        .as_deref()
        .map(|d| ctx.render_template(d))
        .transpose()?
        .unwrap_or_default();
    let rendered_maintainer = cfg
        .maintainer
        .as_deref()
        .map(|m| ctx.render_template(m))
        .transpose()?
        .unwrap_or_default();
    let rendered_homepage = cfg
        .homepage
        .as_deref()
        .map(|h| ctx.render_template(h))
        .transpose()?
        .unwrap_or_default();
    let rendered_license = cfg
        .license
        .as_deref()
        .map(|l| ctx.render_template(l))
        .transpose()?
        .unwrap_or_default();
    let rendered_script = ctx.render_template(script)?;
    let rendered_compression = cfg
        .compression
        .as_deref()
        .map(|c| ctx.render_template(c))
        .transpose()?;

    let keywords: Vec<String> = cfg
        .keywords
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|k| ctx.render_template(k))
        .collect::<Result<Vec<_>>>()?;

    let extra_args: Vec<String> = cfg
        .extra_args
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|a| ctx.render_template(a))
        .collect::<Result<Vec<_>>>()?;

    let lsm = Lsm {
        title: rendered_name.clone(),
        version: version.to_string(),
        description: rendered_description,
        keywords,
        maintained_by: rendered_maintainer,
        primary_site: rendered_homepage,
        platform: platform.to_string(),
        copying_policy: rendered_license,
    };

    // Disambiguate the staging dir per amd64 variant so two non-baseline
    // variants of one platform don't stage into (and clobber) the same dir.
    let work_subdir = match amd64_variant {
        Some(v) if v != "v1" => format!("{platform}_{v}"),
        _ => platform.to_string(),
    };
    let work_dir = dist.join("makeself").join(id).join(work_subdir);

    if dry_run {
        log.status(&format!(
            "(dry-run) would create makeself package {}",
            filename
        ));
        return Ok(());
    }

    let job_binaries: Vec<(std::path::PathBuf, String)> = binaries
        .iter()
        .map(|b| (b.path.clone(), b.name.clone()))
        .collect();

    let job_extra_files: Vec<(std::path::PathBuf, String)> = resolve_extra_file_pairs(cfg);

    let script_path = Path::new(&rendered_script).to_path_buf();
    let script_basename = script_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("setup.sh")
        .to_string();

    let lsm_text = lsm.render();

    jobs.push(MakeselfJob {
        id: id.to_string(),
        filename,
        work_dir,
        output_path,
        rendered_name,
        script_src: script_path,
        script_basename,
        rendered_compression,
        extra_args,
        lsm_text,
        binaries: job_binaries,
        extra_files: job_extra_files,
        primary_target: primary.target.clone(),
        primary_crate_name: primary.crate_name.clone(),
        primary_replaces: primary.metadata.get("replaces").cloned(),
        amd64_variant: amd64_variant.map(str::to_string),
    });

    Ok(())
}

/// Pre-compute `(source_path, dest_name)` pairs for extra files.
fn resolve_extra_file_pairs(
    cfg: &anodizer_core::config::MakeselfConfig,
) -> Vec<(std::path::PathBuf, String)> {
    let Some(ref files) = cfg.files else {
        return Vec::new();
    };
    files
        .iter()
        .map(|f| {
            let src = Path::new(&f.source);
            let dest_name = if let Some(ref dst) = f.destination {
                dst.clone()
            } else if f.strip_parent.unwrap_or(false) {
                src.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&f.source)
                    .to_string()
            } else {
                f.source.clone()
            };
            (src.to_path_buf(), dest_name)
        })
        .collect()
}

/// Environment requirements for the makeself stage: the `makeself` binary
/// whenever any `makeselfs:` entry is active (entries whose `skip`
/// evaluates true are inert).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = ctx.config.makeselfs.iter().any(|cfg| {
        !cfg.skip.as_ref().is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
    });
    if !any {
        return Vec::new();
    }
    vec![anodizer_core::EnvRequirement::Tool {
        name: "makeself".to_string(),
    }]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::MakeselfConfig;
    use std::path::PathBuf;

    #[test]
    fn default_name_template_contains_full_micro_arch_suffix() {
        // Guard the de-smell: the makeself default must compose from the shared
        // full-suffix const, not re-embed the Arm/Mips/Amd64 clause literal.
        assert!(
            default_name_template()
                .contains(anodizer_core::archive_name::MICRO_ARCH_VARIANT_SUFFIX),
            "makeself default must reuse MICRO_ARCH_VARIANT_SUFFIX: {}",
            default_name_template()
        );
    }

    #[test]
    fn test_lsm_render() {
        let lsm = Lsm {
            title: "MyApp".to_string(),
            version: "1.0.0".to_string(),
            description: "A test application".to_string(),
            keywords: vec!["test".to_string(), "app".to_string()],
            maintained_by: "Test User".to_string(),
            primary_site: "https://example.com".to_string(),
            platform: "linux_amd64".to_string(),
            copying_policy: "MIT".to_string(),
        };
        let rendered = lsm.render();
        assert!(rendered.starts_with("Begin4\n"));
        assert!(rendered.ends_with("End"));
        assert!(rendered.contains("Title: MyApp"));
        assert!(rendered.contains("Version: 1.0.0"));
        assert!(rendered.contains("Keywords: test, app"));
        assert!(rendered.contains("Copying-policy: MIT"));
    }

    #[test]
    fn test_make_args_default_compression() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        assert_eq!(args[0], "--quiet");
        assert!(args.contains(&"--lsm".to_string()));
        assert!(args.contains(&"package.lsm".to_string()));
        assert!(args.contains(&".".to_string()));
        assert!(args.contains(&"myapp.run".to_string()));
        assert!(args.contains(&"MyApp".to_string()));
        assert!(args.contains(&"./setup.sh".to_string()));
        assert!(
            args.contains(&"--xz".to_string()),
            "compression must default to --xz, not makeself's --gzip default: {args:?}"
        );
    }

    #[test]
    fn test_make_args_target_pinned() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        let idx = args
            .iter()
            .position(|a| a == "--target")
            .expect("expected --target in args");
        assert_eq!(
            args[idx + 1],
            "myapp",
            "target must derive from filename without .run extension"
        );
    }

    #[test]
    fn test_make_args_target_handles_filename_without_run_ext() {
        let args = make_args("MyApp", "weird-name", None, "./setup.sh", &[], None);
        let idx = args
            .iter()
            .position(|a| a == "--target")
            .expect("expected --target in args");
        assert_eq!(args[idx + 1], "weird-name");
    }

    #[test]
    fn test_make_args_xz_compression() {
        let args = make_args("MyApp", "myapp.run", Some("xz"), "./setup.sh", &[], None);
        assert!(args.contains(&"--xz".to_string()));
    }

    #[test]
    fn test_make_args_gzip_passthrough() {
        // Explicit gzip is honored even though it's non-deterministic — user's
        // choice respected; they can fix by setting compression: xz.
        let args = make_args("MyApp", "myapp.run", Some("gzip"), "./setup.sh", &[], None);
        assert!(args.contains(&"--gzip".to_string()));
    }

    #[test]
    fn test_make_args_no_compression() {
        let args = make_args("MyApp", "myapp.run", Some("none"), "./setup.sh", &[], None);
        assert!(args.contains(&"--nocomp".to_string()));
    }

    #[test]
    fn test_make_args_extra_args() {
        let extra = vec!["--noprogress".to_string(), "--nox11".to_string()];
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &extra, None);
        assert!(args.contains(&"--noprogress".to_string()));
        assert!(args.contains(&"--nox11".to_string()));
    }

    #[test]
    fn test_make_args_packaging_date_emitted() {
        let args = make_args(
            "MyApp",
            "myapp.run",
            None,
            "./setup.sh",
            &[],
            Some("Tue May 20 14:30:00 UTC 2026"),
        );
        let idx = args
            .iter()
            .position(|a| a == "--packaging-date")
            .expect("expected --packaging-date in args");
        assert_eq!(args[idx + 1], "Tue May 20 14:30:00 UTC 2026");
    }

    #[test]
    fn test_make_args_packaging_date_omitted_when_none() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        assert!(!args.iter().any(|a| a == "--packaging-date"));
    }

    #[test]
    fn test_resolve_packaging_date_honors_sde() {
        let env =
            anodizer_core::env_source::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let date = resolve_packaging_date_with_env(&env).expect("packaging date under SDE");
        // 1715000000 = 2024-05-06 16:53:20 UTC; format mirrors `LC_ALL=C date -u`.
        assert!(date.contains("2024"), "date string: {date}");
        assert!(date.contains("UTC"), "date string: {date}");
    }

    #[test]
    fn test_resolve_packaging_date_none_without_sde() {
        let env = anodizer_core::env_source::MapEnvSource::new();
        assert!(resolve_packaging_date_with_env(&env).is_none());
    }

    #[test]
    fn test_group_by_platform() {
        let a1 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let a2 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp-darwin"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let artifacts = vec![a1, a2];
        let groups = group_by_platform(&artifacts);
        assert_eq!(groups.len(), 2);
        // Neither binary carries amd64_variant metadata, so the variant half of
        // the key is None for both.
        assert!(groups.contains_key(&("linux_amd64".to_string(), None)));
        assert!(groups.contains_key(&("darwin_arm64".to_string(), None)));
    }

    #[test]
    fn test_makeself_stage_skips_empty_configs() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        let stage = MakeselfStage;
        // No makeself configs → no-op
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_makeself_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
makeselfs:
  - id: default
    script: install.sh
    compression: xz
    os:
      - linux
    files:
      - src: README.md
        dst: README.md
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        let ms = &config.makeselfs[0];
        assert_eq!(ms.id.as_deref(), Some("default"));
        assert_eq!(ms.script.as_deref(), Some("install.sh"));
        assert_eq!(ms.compression.as_deref(), Some("xz"));
        assert_eq!(ms.os.as_ref().unwrap(), &["linux"]);
        assert_eq!(ms.files.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_makeself_config_single_object() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
makeselfs:
  script: install.sh
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        assert_eq!(config.makeselfs[0].script.as_deref(), Some("install.sh"));
    }

    #[test]
    fn test_makeself_requires_script() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("test".to_string()),
            ..Default::default()
        }];
        let stage = MakeselfStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("script"),
            "should require script field"
        );
    }

    // ---- additional coverage ----

    #[test]
    fn test_lsm_render_skips_empty_fields() {
        let lsm = Lsm {
            title: "X".into(),
            version: "0.1.0".into(),
            description: String::new(),
            keywords: vec![],
            maintained_by: String::new(),
            primary_site: String::new(),
            platform: "linux_amd64".into(),
            copying_policy: String::new(),
        };
        let r = lsm.render();
        assert!(r.contains("Title: X"));
        assert!(!r.contains("Description:"), "empty Description omitted");
        assert!(!r.contains("Keywords:"), "empty Keywords omitted");
        assert!(!r.contains("Author:"), "empty maintainer omits Author");
    }

    #[test]
    fn test_make_args_bzip2_compression() {
        let args = make_args("App", "app.run", Some("bzip2"), "./s.sh", &[], None);
        assert!(args.contains(&"--bzip2".to_string()));
    }

    #[test]
    fn test_make_args_lzo_compression() {
        let args = make_args("App", "app.run", Some("lzo"), "./s.sh", &[], None);
        assert!(args.contains(&"--lzo".to_string()));
    }

    #[test]
    fn test_make_args_compress_compression() {
        let args = make_args("App", "app.run", Some("compress"), "./s.sh", &[], None);
        assert!(args.contains(&"--compress".to_string()));
    }

    #[test]
    fn test_make_args_unknown_compression_does_not_set_flag() {
        // Unknown values should pass through to makeself's default (no flag
        // appended). Specifically, no compression-related flag should be
        // added when the user passes something unrecognised.
        let args = make_args("App", "app.run", Some("zstd"), "./s.sh", &[], None);
        for flag in [
            "--xz",
            "--gzip",
            "--bzip2",
            "--lzo",
            "--compress",
            "--nocomp",
        ] {
            assert!(
                !args.iter().any(|a| a == flag),
                "unknown compression must not pick a known flag, got {flag} in {args:?}"
            );
        }
    }

    #[test]
    fn test_make_args_script_appears_last_after_extras() {
        let extra = vec!["--noprogress".to_string()];
        let args = make_args("App", "app.run", None, "./setup.sh", &extra, None);
        // Final 4 positionals: archive_dir, output_file, label, startup_script.
        assert_eq!(
            &args[args.len() - 4..],
            &[
                ".".to_string(),
                "app.run".to_string(),
                "App".to_string(),
                "./setup.sh".to_string(),
            ]
        );
    }

    #[test]
    fn test_group_by_platform_unknown_target() {
        let a = Artifact {
            kind: ArtifactKind::Binary,
            name: "no-target".to_string(),
            path: PathBuf::from("/dist/no-target"),
            target: None,
            crate_name: "x".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let inputs = [a];
        let groups = group_by_platform(&inputs);
        assert!(groups.contains_key(&("unknown".to_string(), None)));
    }

    #[test]
    fn test_group_by_platform_preserves_iteration_order() {
        // BTreeMap iteration is alphabetical by key — pin that contract
        // so artifacts.json doesn't drift between runs.
        let a1 = Artifact {
            kind: ArtifactKind::Binary,
            name: "app".into(),
            path: PathBuf::from("/dist/app-1"),
            target: Some("aarch64-apple-darwin".into()),
            crate_name: "app".into(),
            metadata: HashMap::new(),
            size: None,
        };
        let a2 = Artifact {
            kind: ArtifactKind::Binary,
            name: "app".into(),
            path: PathBuf::from("/dist/app-2"),
            target: Some("x86_64-unknown-linux-gnu".into()),
            crate_name: "app".into(),
            metadata: HashMap::new(),
            size: None,
        };
        let inputs = [a1, a2];
        let groups = group_by_platform(&inputs);
        let keys: Vec<_> = groups.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                ("darwin_arm64".to_string(), None),
                ("linux_amd64".to_string(), None),
            ]
        );
    }

    #[test]
    fn test_makeself_same_arch_variants_get_distinct_run_names() {
        // Three x86_64 builds tagged amd64_variant v1/v2/v3 plus one aarch64
        // build must each produce a distinct `.run` job: the default filename
        // appends the amd64 micro-arch suffix (v1 baseline → no suffix, v2 →
        // `…v2…`, v3 → `…v3…`), so the same triple no longer clobbers itself.
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.project_name = "myapp".to_string();
        ctx.config.dist = tmp.path().to_path_buf();
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.0.0");

        for variant in ["v1", "v2", "v3"] {
            let p = tmp.path().join(format!("myapp_{variant}"));
            std::fs::write(&p, b"x").unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: "myapp".to_string(),
                path: p,
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }
        let arm = tmp.path().join("myapp_arm");
        std::fs::write(&arm, b"x").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: arm,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let script = tmp.path().join("install.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();

        let cfg = MakeselfConfig {
            id: Some("default".to_string()),
            script: Some(script.to_string_lossy().into_owned()),
            os: Some(vec!["linux".to_string()]),
            ..Default::default()
        };

        let log =
            anodizer_core::log::StageLogger::new("makeself", anodizer_core::log::Verbosity::Normal);
        let mut jobs: Vec<MakeselfJob> = Vec::new();
        let mut arch_guard = ArchPathGuard::new();
        collect_makeself_config_jobs(
            &mut ctx,
            &log,
            &cfg,
            tmp.path(),
            "1.0.0",
            "myapp",
            false,
            &mut arch_guard,
            &mut jobs,
        )
        .unwrap();

        assert_eq!(jobs.len(), 4, "expected one job per variant + arm64");
        let names: Vec<&str> = jobs.iter().map(|j| j.filename.as_str()).collect();
        let distinct: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(
            distinct.len(),
            names.len(),
            "all .run filenames must be distinct: {names:?}"
        );
        // The v1 baseline keeps the historical unsuffixed name; v2/v3 carry the
        // variant; arm64 renders its own arch with no amd64 suffix.
        assert!(names.contains(&"myapp_1.0.0_linux_amd64.run"));
        assert!(names.contains(&"myapp_1.0.0_linux_amd64v2.run"));
        assert!(names.contains(&"myapp_1.0.0_linux_amd64v3.run"));
        assert!(names.contains(&"myapp_1.0.0_linux_arm64.run"));

        // Each job carries the binary's amd64_variant so the produced `.run`
        // artifact can be told apart from its sibling amd64 builds.
        let v3_job = jobs
            .iter()
            .find(|j| j.filename == "myapp_1.0.0_linux_amd64v3.run")
            .expect("v3 job must exist");
        assert_eq!(v3_job.amd64_variant.as_deref(), Some("v3"));

        // The artifact metadata mirrors that variant.
        let v3_meta = makeself_artifact_metadata(v3_job);
        assert_eq!(v3_meta.get("amd64_variant").map(String::as_str), Some("v3"));
        assert_eq!(v3_meta.get("format").map(String::as_str), Some("makeself"));

        // A v1 build carries its variant verbatim, mirroring the flatpak /
        // appimage stages (the disambiguator lives in the name, not by
        // stripping v1 from metadata).
        let baseline_job = jobs
            .iter()
            .find(|j| j.filename == "myapp_1.0.0_linux_amd64.run")
            .expect("baseline job must exist");
        assert_eq!(baseline_job.amd64_variant.as_deref(), Some("v1"));
        assert_eq!(
            makeself_artifact_metadata(baseline_job)
                .get("amd64_variant")
                .map(String::as_str),
            Some("v1")
        );

        // A non-amd64 build carries no amd64_variant at all.
        let arm_job = jobs
            .iter()
            .find(|j| j.filename == "myapp_1.0.0_linux_arm64.run")
            .expect("arm64 job must exist");
        assert_eq!(arm_job.amd64_variant, None);
        assert!(!makeself_artifact_metadata(arm_job).contains_key("amd64_variant"));
    }

    #[test]
    fn test_makeself_constant_filename_bails_across_variants() {
        // A constant user `filename:` lacking any arch/variant discriminator
        // renders the same `.run` path for two amd64 variants — the
        // ArchPathGuard must error loudly instead of letting the second job
        // silently overwrite the first.
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.project_name = "myapp".to_string();
        ctx.config.dist = tmp.path().to_path_buf();
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.0.0");

        for variant in ["v1", "v3"] {
            let p = tmp.path().join(format!("myapp_{variant}"));
            std::fs::write(&p, b"x").unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: "myapp".to_string(),
                path: p,
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }

        let script = tmp.path().join("install.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();

        let cfg = MakeselfConfig {
            id: Some("default".to_string()),
            script: Some(script.to_string_lossy().into_owned()),
            os: Some(vec!["linux".to_string()]),
            // Constant — no `{{ .Arch }}` / `{{ .Amd64 }}` discriminator.
            filename: Some("myapp-installer".to_string()),
            ..Default::default()
        };

        let log =
            anodizer_core::log::StageLogger::new("makeself", anodizer_core::log::Verbosity::Normal);
        let mut jobs: Vec<MakeselfJob> = Vec::new();
        let mut arch_guard = ArchPathGuard::new();
        let err = collect_makeself_config_jobs(
            &mut ctx,
            &log,
            &cfg,
            tmp.path(),
            "1.0.0",
            "myapp",
            false,
            &mut arch_guard,
            &mut jobs,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("makeself:"), "{err}");
        assert!(err.contains("crate 'myapp'"), "{err}");
        assert!(err.contains("{{ .Amd64 }}"), "{err}");
    }

    #[test]
    fn test_makeself_stage_dry_run_does_not_invoke_makeself() {
        // Dry-run must not shell out and must not produce an artifact.
        let tmp = tempfile::tempdir().unwrap();
        let binary_path = tmp.path().join("bin");
        std::fs::write(&binary_path, b"x").unwrap();
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.config.project_name = "proj".to_string();
        ctx.config.dist = tmp.path().to_path_buf();
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some("install.sh".into()),
            ..Default::default()
        }];
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "bin".into(),
            path: binary_path,
            target: Some("x86_64-unknown-linux-gnu".into()),
            crate_name: "proj".into(),
            metadata: HashMap::new(),
            size: None,
        });
        let stage = MakeselfStage;
        stage.run(&mut ctx).expect("dry-run should succeed");
        assert!(
            ctx.artifacts
                .all()
                .iter()
                .all(|a| a.kind != ArtifactKind::Makeself),
            "dry-run must not register a Makeself artifact"
        );
    }

    /// Two `makeselfs:` configs (distinct `id`, both the default filename)
    /// render the same `.run` path for one platform. The guard now spans every
    /// config of the project, so the second config bails loudly instead of
    /// silently clobbering the first config's installer.
    #[test]
    fn test_makeself_two_configs_same_default_name_bail_across_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let binary_path = tmp.path().join("bin");
        std::fs::write(&binary_path, b"x").unwrap();
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.config.project_name = "proj".to_string();
        ctx.config.dist = tmp.path().to_path_buf();
        ctx.config.makeselfs = vec![
            MakeselfConfig {
                id: Some("first".into()),
                script: Some("install.sh".into()),
                os: Some(vec!["linux".into()]),
                ..Default::default()
            },
            MakeselfConfig {
                id: Some("second".into()),
                script: Some("install.sh".into()),
                os: Some(vec!["linux".into()]),
                ..Default::default()
            },
        ];
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "bin".into(),
            path: binary_path,
            target: Some("x86_64-unknown-linux-gnu".into()),
            crate_name: "proj".into(),
            metadata: HashMap::new(),
            size: None,
        });

        let err = MakeselfStage.run(&mut ctx).unwrap_err().to_string();
        assert!(err.contains("makeself:"), "{err}");
        assert!(err.contains("crate 'proj'"), "{err}");
        assert!(err.contains("{{ .Arch }}"), "{err}");
    }

    #[test]
    fn test_makeself_skip_template_skips_config() {
        // `skip: true` short-circuits before any binary lookup; the
        // missing-script error must NOT fire.
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.project_name = "proj".into();
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("skipme".into()),
            // No script -> would normally bail. skip=true must short-circuit first.
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        }];
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");
        let stage = MakeselfStage;
        stage.run(&mut ctx).expect("skip:true must succeed");
    }

    #[test]
    fn test_makeself_duplicate_ids_rejected() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.makeselfs = vec![
            MakeselfConfig {
                id: Some("dup".into()),
                script: Some("a.sh".into()),
                ..Default::default()
            },
            MakeselfConfig {
                id: Some("dup".into()),
                script: Some("b.sh".into()),
                ..Default::default()
            },
        ];
        let err = MakeselfStage.run(&mut ctx).unwrap_err().to_string();
        assert!(err.contains("duplicate id"), "{err}");
    }

    #[test]
    fn test_makeself_no_matching_binaries_bails() {
        // Configured for linux/darwin but only a windows artifact exists.
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.project_name = "proj".into();
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some("install.sh".into()),
            ..Default::default()
        }];
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "bin.exe".into(),
            path: PathBuf::from("/dist/bin.exe"),
            target: Some("x86_64-pc-windows-msvc".into()),
            crate_name: "proj".into(),
            metadata: HashMap::new(),
            size: None,
        });
        let err = MakeselfStage.run(&mut ctx).unwrap_err().to_string();
        assert!(err.contains("no binaries found"), "{err}");
    }

    /// Per-crate determinism (`--crate cfgd-csi`): only one crate's binaries
    /// are built, so a config scoped to a DIFFERENT crate (cfgd's
    /// `ids: [cfgd]`) matches nothing. Under per-crate scope this must SKIP,
    /// not bail — the config legitimately doesn't apply to this crate.
    #[test]
    fn test_makeself_empty_match_skips_when_per_crate_scoped() {
        let options = anodizer_core::context::ContextOptions {
            selected_crates: vec!["cfgd-csi".into()],
            ..Default::default()
        };
        let mut ctx = Context::new(anodizer_core::config::Config::default(), options);
        ctx.config.project_name = "cfgd-csi".into();
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("default".into()),
            ids: Some(vec!["cfgd".into()]),
            script: Some("install.sh".into()),
            ..Default::default()
        }];
        ctx.template_vars_mut().set("ProjectName", "cfgd-csi");
        ctx.template_vars_mut().set("Version", "1.0.0");
        // Only the foreign crate's binary exists (cfgd-csi), so `ids: [cfgd]`
        // matches nothing.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "cfgd-csi".into(),
            path: PathBuf::from("/dist/cfgd-csi"),
            target: Some("x86_64-unknown-linux-gnu".into()),
            crate_name: "cfgd-csi".into(),
            metadata: HashMap::new(),
            size: None,
        });
        MakeselfStage
            .run(&mut ctx)
            .expect("per-crate-scoped empty match must skip, not bail");
        assert!(
            ctx.artifacts
                .all()
                .iter()
                .all(|a| a.kind != ArtifactKind::Makeself),
            "skipped config must register no Makeself artifact"
        );
    }

    /// Target-restricted (`--targets`) build: the built target set is partial
    /// by construction, so a config whose os/arch can't match the slice must
    /// SKIP (mirrors emission-validate / sbom under `--targets`).
    #[test]
    fn test_makeself_empty_match_skips_when_target_restricted() {
        let options = anodizer_core::context::ContextOptions {
            partial_target: Some(anodizer_core::partial::PartialTarget::Targets(vec![
                "x86_64-pc-windows-msvc".into(),
            ])),
            ..Default::default()
        };
        let mut ctx = Context::new(anodizer_core::config::Config::default(), options);
        ctx.config.project_name = "proj".into();
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some("install.sh".into()),
            ..Default::default()
        }];
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");
        // Only a windows binary was built (the --targets slice), so the
        // default linux/darwin os filter matches nothing.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "proj.exe".into(),
            path: PathBuf::from("/dist/proj.exe"),
            target: Some("x86_64-pc-windows-msvc".into()),
            crate_name: "proj".into(),
            metadata: HashMap::new(),
            size: None,
        });
        MakeselfStage
            .run(&mut ctx)
            .expect("target-restricted empty match must skip, not bail");
    }

    #[test]
    fn test_resolve_packaging_date_format_matches_lc_all_c_date() {
        // The format string is `%a %b %e %H:%M:%S UTC %Y` — assert the
        // structure of a known epoch matches that exactly.
        let env =
            anodizer_core::env_source::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1577836800");
        let date = resolve_packaging_date_with_env(&env).expect("epoch set");
        // 1577836800 = 2020-01-01 00:00:00 UTC
        assert!(date.contains("2020"), "{date}");
        assert!(date.contains("Jan"), "{date}");
        assert!(date.contains("UTC"), "{date}");
    }

    // ---- live `makeself` subprocess run path (PATH-stub harness) ----
    //
    // makeself is hard-coded as `Command::new("makeself")` (no configurable
    // `cmd:` field), so every test that drives the real run path prepends a
    // stub dir to `PATH` via `FakeToolDir::activate` and is `#[serial]`.

    use anodizer_core::test_helpers::fake_tool::FakeToolDir;

    /// A scratch project laid out for a live makeself run: a `dist` root, an
    /// on-disk binary artifact, and an on-disk startup script. Returns the
    /// pieces a test needs to build a `Context` and assert against `dist`.
    struct MakeselfFixture {
        _tmp: tempfile::TempDir,
        dist: PathBuf,
        binary_path: PathBuf,
        script_path: PathBuf,
    }

    impl MakeselfFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dist = tmp.path().join("dist");
            std::fs::create_dir_all(&dist).unwrap();
            let binary_path = tmp.path().join("bin");
            std::fs::write(&binary_path, b"\x7fELF-fake-binary").unwrap();
            let script_path = tmp.path().join("install.sh");
            std::fs::write(&script_path, b"#!/bin/sh\necho hi\n").unwrap();
            Self {
                _tmp: tmp,
                dist,
                binary_path,
                script_path,
            }
        }

        /// Build a Context whose `dist`, `project_name`, and template vars are
        /// wired, plus one binary artifact per `(target, crate_name)` pair.
        fn ctx(&self, binaries: &[(&str, &str)]) -> Context {
            let mut ctx = Context::new(
                anodizer_core::config::Config::default(),
                anodizer_core::context::ContextOptions::default(),
            );
            ctx.config.project_name = "proj".to_string();
            ctx.config.dist = self.dist.clone();
            ctx.template_vars_mut().set("ProjectName", "proj");
            ctx.template_vars_mut().set("Version", "1.0.0");
            for (i, (target, crate_name)) in binaries.iter().enumerate() {
                let p = if i == 0 {
                    self.binary_path.clone()
                } else {
                    let extra = self._tmp.path().join(format!("bin{i}"));
                    std::fs::write(&extra, b"\x7fELF-fake-binary").unwrap();
                    extra
                };
                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Binary,
                    name: "bin".into(),
                    path: p,
                    target: Some((*target).to_string()),
                    crate_name: (*crate_name).to_string(),
                    metadata: HashMap::new(),
                    size: None,
                });
            }
            ctx
        }
    }

    /// Default-template output filename for the single-config linux_amd64 path:
    /// `proj_1.0.0_linux_amd64.run`, plus its work-dir-relative .run name (the
    /// stub `.creates()` writes the built archive into the job work dir, where
    /// `execute_makeself_job` renames it out to `dist/<filename>`).
    fn linux_amd64_run_name() -> String {
        "proj_1.0.0_linux_amd64.run".to_string()
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_invokes_makeself_and_records_artifact() {
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("live makeself run should succeed");

        // The stage shelled out to `makeself` exactly once.
        assert_eq!(tools.call_count("makeself"), 1);

        // Output landed at dist/<filename> (renamed out of the work dir).
        let out = fx.dist.join(linux_amd64_run_name());
        assert!(
            out.exists(),
            "installer .run not produced at {}",
            out.display()
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "#!installer");

        // Exactly one Makeself artifact, format=makeself, path = the .run.
        let made: Vec<&Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Makeself)
            .collect();
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].name, linux_amd64_run_name());
        assert_eq!(made[0].path, out);
        assert_eq!(
            made[0].metadata.get("format").map(String::as_str),
            Some("makeself")
        );
        assert_eq!(made[0].metadata.get("id").map(String::as_str), Some("d"));
        assert_eq!(made[0].target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_argv_shape() {
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            extra_args: Some(vec!["--noprogress".into()]),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage.run(&mut ctx).expect("run should succeed");

        let argv = &tools.calls("makeself")[0];
        // --quiet leads; --lsm package.lsm; --target <name-sans-.run>.
        assert_eq!(argv[0], "--quiet");
        let lsm = argv
            .iter()
            .position(|a| a == "--lsm")
            .expect("--lsm present");
        assert_eq!(argv[lsm + 1], "package.lsm");
        let tgt = argv
            .iter()
            .position(|a| a == "--target")
            .expect("--target present");
        assert_eq!(argv[tgt + 1], "proj_1.0.0_linux_amd64");
        // Default compression is --xz (not makeself's gzip default).
        assert!(argv.iter().any(|a| a == "--xz"), "{argv:?}");
        // Extra args carried through.
        assert!(argv.iter().any(|a| a == "--noprogress"), "{argv:?}");
        // Final four positionals: archive_dir output_file label startup_script.
        assert_eq!(
            &argv[argv.len() - 4..],
            &[
                ".".to_string(),
                linux_amd64_run_name(),
                "proj".to_string(),
                "./install.sh".to_string(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_stages_files_into_work_dir() {
        // The work dir must contain the copied binary, the copied startup
        // script, the LSM file, and (via `files:`) an extra file at its
        // mapped destination — all staged before makeself is invoked.
        let fx = MakeselfFixture::new();
        let extra_src = fx._tmp.path().join("README.md");
        std::fs::write(&extra_src, b"readme body").unwrap();

        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            files: Some(vec![anodizer_core::config::MakeselfFile {
                source: extra_src.to_string_lossy().into_owned(),
                destination: Some("docs/README.md".into()),
                strip_parent: None,
            }]),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage.run(&mut ctx).expect("run should succeed");

        let work = fx.dist.join("makeself").join("d").join("linux_amd64");
        assert!(work.join("bin").exists(), "binary not staged");
        assert!(work.join("install.sh").exists(), "script not staged");
        let lsm = std::fs::read_to_string(work.join("package.lsm")).unwrap();
        assert!(lsm.starts_with("Begin4\n") && lsm.contains("End"), "{lsm}");
        assert_eq!(
            std::fs::read_to_string(work.join("docs/README.md")).unwrap(),
            "readme body",
            "extra file not staged at its mapped destination"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_pins_workdir_mtimes_under_sde() {
        // With SOURCE_DATE_EPOCH set, every staged file's mtime is pinned to
        // that epoch before makeself runs, so tar embeds stable timestamps.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        // activate() and the SDE var both mutate process env — hold the same
        // serialised window; restore SDE on exit.
        let _g = tools.activate();
        let prior_sde = std::env::var_os("SOURCE_DATE_EPOCH");
        // SAFETY: serialised by the env mutex held inside `_g` for this test.
        // env-ok: SOURCE_DATE_EPOCH under #[serial(path_env)] + env_mutex; restored on drop
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1577836800") };

        let run = MakeselfStage.run(&mut ctx);

        // Restore SDE before asserting so a panic doesn't leak it.
        // SAFETY: still inside the `_g` serialised window.
        unsafe {
            match prior_sde {
                // env-ok: SOURCE_DATE_EPOCH under #[serial(path_env)] + env_mutex; restored on drop
                Some(v) => std::env::set_var("SOURCE_DATE_EPOCH", v),
                // env-ok: SOURCE_DATE_EPOCH under #[serial(path_env)] + env_mutex; restored on drop
                None => std::env::remove_var("SOURCE_DATE_EPOCH"),
            }
        }
        run.expect("run under SDE should succeed");

        let work = fx.dist.join("makeself").join("d").join("linux_amd64");
        let mtime = std::fs::metadata(work.join("bin"))
            .unwrap()
            .modified()
            .unwrap();
        let secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 1577836800 = 2020-01-01 00:00:00 UTC.
        assert_eq!(secs, 1_577_836_800, "staged file mtime not pinned to SDE");
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_tool_failure_surfaces_stderr() {
        // Non-zero exit → the stage bails, naming the filename, id, and the
        // tool's stderr/stdout in the error.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("boomcfg".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .stdout("partial-progress\n")
            .stderr("boom: archive failed\n")
            .exit(1)
            .install();
        let _g = tools.activate();

        let err = MakeselfStage.run(&mut ctx).unwrap_err().to_string();
        assert!(err.contains("makeself"), "{err}");
        assert!(err.contains("boomcfg"), "error must name the id: {err}");
        assert!(
            err.contains("boom: archive failed"),
            "error must carry stderr: {err}"
        );
        // No artifact registered on failure.
        assert!(
            ctx.artifacts
                .all()
                .iter()
                .all(|a| a.kind != ArtifactKind::Makeself),
            "failed run must not register a Makeself artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_output_missing_after_success() {
        // makeself exits 0 but produces no .run file in the work dir — the
        // rename/copy of the built archive out to dist must fail.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        // exits 0 but does NOT `.creates()` the .run file.
        tools.tool("makeself").install();
        let _g = tools.activate();

        let err = MakeselfStage.run(&mut ctx).unwrap_err().to_string();
        assert!(
            err.contains("makeself: move"),
            "missing output must fail at the move step: {err}"
        );
        assert!(
            !fx.dist.join(linux_amd64_run_name()).exists(),
            "no installer should exist when makeself emitted nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_multi_platform_one_run_per_platform() {
        // Two targets (linux amd64 + darwin arm64) → two makeself invocations,
        // two .run files, two registered artifacts. Mirrors workspace builds
        // that fan out per platform.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[
            ("x86_64-unknown-linux-gnu", "proj"),
            ("aarch64-apple-darwin", "proj"),
        ]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let darwin_run = "proj_1.0.0_darwin_arm64.run".to_string();
        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .creates(&darwin_run, "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("multi-platform run should succeed");

        assert_eq!(tools.call_count("makeself"), 2);
        let mut made: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Makeself)
            .map(|a| a.name.clone())
            .collect();
        made.sort();
        assert_eq!(made, vec![darwin_run, linux_amd64_run_name()]);
        assert!(fx.dist.join(linux_amd64_run_name()).exists());
        assert!(fx.dist.join("proj_1.0.0_darwin_arm64.run").exists());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_per_crate_binaries_share_one_platform_run() {
        // Two binaries from different crates but the SAME platform group into a
        // single .run (one makeself invocation), with both staged into the work
        // dir. Covers the workspace per-crate mode where multiple crates emit
        // linux/amd64 binaries.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[
            ("x86_64-unknown-linux-gnu", "crate-a"),
            ("x86_64-unknown-linux-gnu", "crate-b"),
        ]);
        // Distinct names so both copy into the work dir without clobbering.
        let all: Vec<Artifact> = ctx.artifacts.all().to_vec();
        ctx.artifacts = anodizer_core::artifact::ArtifactRegistry::default();
        for (i, mut a) in all.into_iter().enumerate() {
            a.name = format!("bin-{i}");
            ctx.artifacts.add(a);
        }
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("per-crate run should succeed");

        // One platform → one invocation, one artifact.
        assert_eq!(tools.call_count("makeself"), 1);
        let work = fx.dist.join("makeself").join("d").join("linux_amd64");
        assert!(work.join("bin-0").exists(), "crate-a binary not staged");
        assert!(work.join("bin-1").exists(), "crate-b binary not staged");
        assert_eq!(
            ctx.artifacts
                .all()
                .iter()
                .filter(|a| a.kind == ArtifactKind::Makeself)
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_custom_filename_template_and_compression() {
        // A templated `filename:` (without .run) gets `.run` appended; an
        // explicit `compression: gzip` lands `--gzip` in argv; the output
        // is written under the rendered name.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            filename: Some("{{ ProjectName }}-installer-{{ Os }}".into()),
            compression: Some("gzip".into()),
            ..Default::default()
        }];

        let rendered = "proj-installer-linux.run".to_string();
        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(&rendered, "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("templated filename run should succeed");

        let argv = &tools.calls("makeself")[0];
        assert!(
            argv.iter().any(|a| a == "--gzip"),
            "explicit gzip honored: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == &rendered),
            "rendered filename in argv: {argv:?}"
        );
        assert!(
            fx.dist.join(&rendered).exists(),
            "installer not written at rendered filename"
        );
        let made = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.kind == ArtifactKind::Makeself)
            .expect("makeself artifact");
        assert_eq!(made.name, rendered);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_emits_replaces_metadata() {
        // A binary carrying `replaces` metadata propagates it onto the
        // resulting Makeself artifact.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        // Re-stamp the single artifact with a `replaces` metadata entry.
        let mut a = ctx.artifacts.all()[0].clone();
        a.metadata.insert("replaces".into(), "oldpkg".into());
        ctx.artifacts = anodizer_core::artifact::ArtifactRegistry::default();
        ctx.artifacts.add(a);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage.run(&mut ctx).expect("run should succeed");

        let made = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.kind == ArtifactKind::Makeself)
            .expect("makeself artifact");
        assert_eq!(
            made.metadata.get("replaces").map(String::as_str),
            Some("oldpkg")
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_arch_filter_narrows_platforms() {
        // `arch: [amd64]` drops the arm64 binary; only the amd64 .run is built.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[
            ("x86_64-unknown-linux-gnu", "proj"),
            ("aarch64-unknown-linux-musl", "proj"),
        ]);
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("d".into()),
            script: Some(fx.script_path.to_string_lossy().into_owned()),
            arch: Some(vec!["amd64".into()]),
            ..Default::default()
        }];

        let tools = FakeToolDir::new();
        tools
            .tool("makeself")
            .creates(linux_amd64_run_name(), "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("arch-filtered run should succeed");

        assert_eq!(tools.call_count("makeself"), 1);
        let made: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Makeself)
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(made, vec![linux_amd64_run_name()]);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn test_makeself_live_run_two_configs_distinct_ids() {
        // Two configs (distinct ids AND distinct filenames) each emit their own
        // .run with the id in the work-dir path and the artifact metadata. Two
        // invocations. Distinct filenames are required: a per-project guard now
        // spans every config, so two configs rendering the same default name for
        // one platform would (correctly) bail rather than silently clobber.
        let fx = MakeselfFixture::new();
        let mut ctx = fx.ctx(&[("x86_64-unknown-linux-gnu", "proj")]);
        ctx.config.makeselfs = vec![
            MakeselfConfig {
                id: Some("alpha".into()),
                script: Some(fx.script_path.to_string_lossy().into_owned()),
                filename: Some("alpha_{{ .Arch }}".into()),
                ..Default::default()
            },
            MakeselfConfig {
                id: Some("beta".into()),
                script: Some(fx.script_path.to_string_lossy().into_owned()),
                filename: Some("beta_{{ .Arch }}".into()),
                ..Default::default()
            },
        ];

        let tools = FakeToolDir::new();
        // Each config renders a distinct filename, so both coexist.
        tools
            .tool("makeself")
            .creates("alpha_amd64.run", "#!installer")
            .creates("beta_amd64.run", "#!installer")
            .install();
        let _g = tools.activate();

        MakeselfStage
            .run(&mut ctx)
            .expect("two-config run should succeed");

        assert_eq!(tools.call_count("makeself"), 2);
        let ids: std::collections::HashSet<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Makeself)
            .filter_map(|a| a.metadata.get("id").cloned())
            .collect();
        assert!(ids.contains("alpha") && ids.contains("beta"), "{ids:?}");
        assert!(
            fx.dist
                .join("makeself")
                .join("alpha")
                .join("linux_amd64")
                .exists()
        );
        assert!(
            fx.dist
                .join("makeself")
                .join("beta")
                .join("linux_amd64")
                .exists()
        );
    }

    fn seeded_ctx(target: &str, amd64_variant: Option<&str>) -> Context {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.2.3");
        let (os, arch) = anodizer_core::target::map_target(target);
        set_per_target_template_vars(&mut ctx, Some(target), &os, &arch, amd64_variant);
        ctx
    }

    #[test]
    fn default_filename_mips64el_single_arch_token() {
        // Arch already carries the whole mips token; the default template's
        // `{% if Mips %}` clause must add nothing — never the doubled
        // `myapp_1.2.3_linux_mips64el_mips64el.run`.
        let ctx = seeded_ctx("mips64el-unknown-linux-gnuabi64", None);
        let name = resolve_makeself_filename(
            &ctx,
            &default_name_template(),
            "myapp",
            "1.2.3",
            "linux",
            "mips64el",
        )
        .unwrap();
        assert_eq!(name, "myapp_1.2.3_linux_mips64el.run");
    }

    #[test]
    fn default_filename_armv7_single_arch_token() {
        // Arch carries the composite armv7 token, so the default template's
        // `{% if Arm %}v{{ Arm }}` clause must add nothing — never the doubled
        // `myapp_1.2.3_linux_armv7v7.run` (same class as the mips guard).
        let ctx = seeded_ctx("armv7-unknown-linux-gnueabihf", None);
        let name = resolve_makeself_filename(
            &ctx,
            &default_name_template(),
            "myapp",
            "1.2.3",
            "linux",
            "armv7",
        )
        .unwrap();
        assert_eq!(name, "myapp_1.2.3_linux_armv7.run");
    }

    #[test]
    fn default_filename_armv6_single_arch_token() {
        let ctx = seeded_ctx("armv6-unknown-linux-gnueabihf", None);
        let name = resolve_makeself_filename(
            &ctx,
            &default_name_template(),
            "myapp",
            "1.2.3",
            "linux",
            "armv6",
        )
        .unwrap();
        assert_eq!(name, "myapp_1.2.3_linux_armv6.run");
    }

    #[test]
    fn untagged_x86_64_amd64_matches_build_baseline() {
        // Same value the build stage seeds for an untagged x86_64 binary name
        // ("v1"), and the default filename stays suffix-free (v1 baseline).
        let ctx = seeded_ctx("x86_64-unknown-linux-gnu", None);
        assert_eq!(
            ctx.template_vars().get("Amd64").map(String::as_str),
            Some("v1")
        );
        let name = resolve_makeself_filename(
            &ctx,
            &default_name_template(),
            "myapp",
            "1.2.3",
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(name, "myapp_1.2.3_linux_amd64.run");
    }
}
