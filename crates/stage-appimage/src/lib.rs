//! AppImage packaging stage.
//!
//! Bundles a built Linux binary plus its desktop integration (a `.desktop`
//! entry + icon, an optional harvested runtime tree, and arbitrary extra
//! files) into a single self-contained, runnable `.AppImage` via
//! [`linuxdeploy`](https://github.com/linuxdeploy/linuxdeploy)'s `appimage`
//! output plugin.
//!
//! One `.AppImage` is produced per matching Linux target, so a multi-arch
//! build yields distinct, non-colliding outputs. The runtime-harvest hook
//! (helix's `hx --grammar fetch`-style step) runs ONCE on the host-native
//! binary — the harvested data (grammars / themes / queries) is
//! architecture-independent, so it is reused for every target's AppImage and
//! also staged at a stable dist path (`dist/.appimage-runtime/<id>/`) so an
//! archive `extra_files` glob can ship the same tree in tarballs.
//!
//! linuxdeploy is invoked as:
//!
//! ```text
//! linuxdeploy --appdir <AppDir> -d <desktop> -i <icon> --output appimage [extra_args...]
//! ```
//!
//! with the env it reads (`VERSION`, `ARCH`, `APP`, `OUTPUT`, and optionally
//! `UPDATE_INFORMATION`) set on the process from config + context.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod appdir;
mod job;

pub(crate) use appdir::*;
pub(crate) use job::*;

/// Map a Rust target triple's architecture to the AppImage `ARCH` token
/// linuxdeploy expects (`x86_64`, `aarch64`, `armhf`, `i686`). Falls back to
/// the anodizer arch label when no AppImage-specific mapping applies.
fn appimage_arch(target: &str) -> String {
    let first = target.split('-').next().unwrap_or("");
    match first {
        "x86_64" => "x86_64".to_string(),
        "aarch64" => "aarch64".to_string(),
        "armv7" | "armv7l" | "arm" | "armv6" | "armv6l" => "armhf".to_string(),
        "i686" | "i586" | "i386" => "i686".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => {
            let (_, arch) = anodizer_core::target::map_target(target);
            arch
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Reject duplicate AppImage config IDs (per-id `default` collapses unkeyed
/// entries onto one slot — same shape as makeself/nfpm validation).
fn validate_unique_ids(configs: &[anodizer_core::config::AppImageConfig]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for cfg in configs {
        let id = cfg.id.as_deref().unwrap_or("default");
        if !seen.insert(id.to_string()) {
            bail!("appimage: duplicate id '{}'", id);
        }
    }
    Ok(())
}

/// Validate the required fields of a single AppImage config.
fn validate_config_fields(cfg: &anodizer_core::config::AppImageConfig, id: &str) -> Result<()> {
    if cfg.desktop.as_deref().unwrap_or("").is_empty() {
        bail!("appimage: 'desktop' is required for config id '{}'", id);
    }
    if cfg.icon.as_deref().unwrap_or("").is_empty() {
        bail!("appimage: 'icon' is required for config id '{}'", id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Binary selection (mirrors makeself's collect_matching_binaries)
// ---------------------------------------------------------------------------

/// Filter and clone the binary artifacts that match an AppImage config's
/// id-filter + os/arch selectors. AppImage is Linux-only, so the os filter
/// defaults to `["linux"]`.
fn collect_matching_binaries(
    ctx: &Context,
    cfg: &anodizer_core::config::AppImageConfig,
    os_filter: &[String],
) -> Vec<Artifact> {
    ctx.artifacts
        .all()
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                ArtifactKind::Binary
                    | ArtifactKind::UploadableBinary
                    | ArtifactKind::UniversalBinary
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

/// Resolve the host-native binary for the runtime-harvest step: the built
/// artifact whose target equals the detected host target. `None` when host
/// detection fails (e.g. `rustc` unavailable) or no built artifact targets
/// the host (a pure cross build) — the caller turns that into a clear error.
///
/// Mirrors `stage-archive::run::resolve_host_binary` (the same host-once
/// pattern the archive completion harvest uses).
fn resolve_host_binary(binaries: &[Artifact]) -> Option<Artifact> {
    let host = anodizer_core::partial::detect_host_target().ok()?;
    binaries
        .iter()
        .find(|b| b.target.as_deref() == Some(host.as_str()))
        .cloned()
}

/// The clear error emitted when a runtime harvest is configured but no
/// host-native artifact exists in the build matrix (pure cross build).
fn host_missing_error(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "appimage: runtime_harvest for config '{id}' must run the freshly-built binary on the \
         host to populate the harvest dir, but no built artifact matches the host target (pure \
         cross build). Add the host target to your build matrix so the harvest binary exists."
    )
}

// ---------------------------------------------------------------------------
// Per-target template vars (mirrors makeself)
// ---------------------------------------------------------------------------

/// Group binary artifacts by `(platform, amd64_variant)` — e.g.
/// `("linux_amd64", Some("v3"))` — so each (os, arch) AND micro-architecture
/// variant yields exactly one AppImage.
///
/// The key carries the binary's `amd64_variant` metadata alongside the os/arch
/// platform string so two amd64 builds of one triple (a baseline `v1` and a
/// `-Ctarget-cpu=x86-64-v3` tune) land in separate groups and produce two
/// distinct `.AppImage` files instead of one silently clobbering the other.
///
/// Uses a `BTreeMap` (not `HashMap`) so iteration order is deterministic
/// across runs: callers register one AppImage Artifact per group, and
/// `HashMap` iteration is randomised per process — the matching
/// `stage-archive`/`stage-makeself` regression shipped per-run drift into
/// `dist/artifacts.json`. This stage shares the same guard.
fn group_by_platform<'a>(
    binaries: &'a [Artifact],
) -> std::collections::BTreeMap<(String, Option<String>), Vec<&'a Artifact>> {
    let mut groups: std::collections::BTreeMap<(String, Option<String>), Vec<&'a Artifact>> =
        std::collections::BTreeMap::new();
    for a in binaries {
        let platform = match &a.target {
            Some(t) => {
                let (os, arch) = anodizer_core::target::map_target(t);
                format!("{os}_{arch}")
            }
            None => "unknown".to_string(),
        };
        let variant = a.metadata.get("amd64_variant").cloned();
        groups.entry((platform, variant)).or_default().push(a);
    }
    groups
}

/// Seed Os / Arch / Target plus the per-target variant template vars so the
/// default filename template renders correctly.
///
/// The variant vars come from the shared
/// [`seed_variant_vars`](anodizer_core::archive_name::seed_variant_vars)
/// policy — the same seeding the build stage applies to binary-name templates,
/// so a user template's `{{ .Amd64 }}` renders identically in both places.
/// `amd64_variant` is the built binary's `amd64_variant` metadata: it replaces
/// the `"v1"` baseline so two amd64 builds of one triple (a baseline and a
/// `-Ctarget-cpu=x86-64-v3` tune) render distinct `.AppImage` names; the
/// default suffix's `!= "v1"` guard keeps the baseline suffix-free.
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

/// The amd64 micro-architecture variant suffix the default AppImage filename
/// appends, rendered from the binary's seeded `Amd64` template var.
///
/// AppImage keeps the whole go-arch in `arch_token` (no arm-split), so amd64 is
/// the only micro-architecture dimension that can collide on one token — hence
/// the amd64-only [`INSTALLER_AMD64_VARIANT_SUFFIX`](anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
/// not the full Arm/Mips/Amd64 clause. A baseline `v1` / `None` renders empty,
/// preserving the historical single-variant name.
fn default_amd64_suffix(ctx: &Context) -> Result<String> {
    ctx.render_template(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX)
}

/// Render the `.AppImage` output filename for one (target, platform) combo.
///
/// Honors `cfg.filename` as a Tera template when set (appending `.AppImage`
/// if absent); otherwise composes `<project>-<version>-<arch>[<amd64>].AppImage`
/// (AppImage is Linux-only, so the os segment is omitted). The arch is the
/// AppImage-flavoured arch token, plus the amd64 micro-architecture variant
/// suffix, so multi-arch AND multi-variant builds for the same project never
/// collide on disk.
///
/// Two `appimages:` configs that differ only by `id` (no custom `filename`)
/// and target the same arch render the same default output name and would
/// clobber on disk — set an explicit `filename:` on each to disambiguate.
/// This matches the sibling makeself stage's default-naming behaviour.
/// The rendered `.AppImage` filename paired with the template that produced it:
/// `(rendered_name, resolved_template)`. The resolved template is the user's
/// `filename:` when set, else the composed default (including the amd64 variant
/// suffix) — exactly the string the [`ArchPathGuard`] cites when it rejects a
/// clobber, so the diagnostic never reports an empty template.
type ResolvedFilename = (String, String);

fn resolve_appimage_filename(
    ctx: &Context,
    name_template: Option<&str>,
    project_name: &str,
    version: &str,
    arch_token: &str,
) -> Result<ResolvedFilename> {
    if let Some(tmpl) = name_template.filter(|t| !t.is_empty()) {
        let rendered = ctx.render_template(tmpl)?;
        let output_name = if rendered.ends_with(".AppImage") {
            rendered
        } else {
            format!("{rendered}.AppImage")
        };
        return Ok((output_name, tmpl.to_string()));
    }
    let amd64_suffix = default_amd64_suffix(ctx)?;
    // The composed default is fully rendered, so it serves as both the produced
    // name and the template the guard cites (no `{{ .Arch }}` placeholder to
    // re-render — the arch token is already substituted in).
    let composed = format!("{project_name}-{version}-{arch_token}{amd64_suffix}.AppImage");
    Ok((composed.clone(), composed))
}

// ---------------------------------------------------------------------------
// Runtime harvest (host-once)
// ---------------------------------------------------------------------------

/// Render the harvest command for a config, binding `{{ .ArtifactPath }}` to
/// the host binary's path and `{{ .HarvestDir }}` to the absolute harvest
/// output dir. The bound vars are cleared immediately after rendering so they
/// never leak into later renders (mirrors completions_gen's
/// `clear_generate_vars`).
fn render_harvest_command(
    ctx: &mut Context,
    command_tmpl: &str,
    host: &Artifact,
    harvest_dir: &Path,
) -> Result<String> {
    let tvars = ctx.template_vars_mut();
    tvars.set("ArtifactPath", &host.path.to_string_lossy());
    tvars.set("HarvestDir", &harvest_dir.to_string_lossy());
    let rendered = ctx.render_template(command_tmpl);
    let tvars = ctx.template_vars_mut();
    tvars.set("ArtifactPath", "");
    tvars.set("HarvestDir", "");
    rendered.with_context(|| format!("appimage: render runtime_harvest command '{command_tmpl}'"))
}

/// Run the rendered harvest command once via `sh -c`, populating
/// `harvest_dir` (created beforehand).
fn run_harvest(cmd: &str, harvest_dir: &Path, log: &anodizer_core::log::StageLogger) -> Result<()> {
    std::fs::create_dir_all(harvest_dir)
        .with_context(|| format!("appimage: create harvest dir {}", harvest_dir.display()))?;
    log.status(&format!("harvesting AppImage runtime via `{cmd}`"));
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("appimage: spawn runtime_harvest command `{cmd}`"))?;
    if !output.status.success() {
        bail!(
            "appimage: runtime_harvest command `{cmd}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AppImageStage
// ---------------------------------------------------------------------------

pub struct AppImageStage;

impl Stage for AppImageStage {
    fn name(&self) -> &str {
        "appimage"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let configs = ctx.config.appimages.clone();
        if configs.is_empty() {
            return Ok(());
        }

        let log = ctx.logger("appimage");
        validate_unique_ids(&configs)?;

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let parallelism = ctx.options.parallelism.max(1);
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        let project_name = ctx.config.project_name.clone();
        // Resolve SOURCE_DATE_EPOCH once in the serial phase so each job
        // carries the value (the parallel phase never touches `std::env`).
        let sde_epoch = ctx
            .env_var("SOURCE_DATE_EPOCH")
            .and_then(|s| s.parse::<i64>().ok());

        // One guard spans every `appimages:` config of the project: two configs
        // with the default (or identical) `filename:` render the same `.AppImage`
        // path for one arch — error loudly across configs instead of letting the
        // second silently clobber the first.
        let mut arch_guard = ArchPathGuard::new();

        let mut jobs: Vec<AppImageJob> = Vec::new();
        for cfg in &configs {
            collect_config_jobs(
                ctx,
                &log,
                cfg,
                &dist,
                &version,
                &project_name,
                sde_epoch,
                dry_run,
                &mut arch_guard,
                &mut jobs,
            )?;
        }

        if jobs.is_empty() {
            return Ok(());
        }

        let verbosity = log.verbosity();
        let built = anodizer_core::parallel::run_parallel_chunks(
            &jobs,
            parallelism,
            "appimage",
            &log,
            |job: &AppImageJob| execute_appimage_job(job, verbosity),
        )?;

        for artifact in built.into_iter().flatten() {
            ctx.artifacts.add(artifact);
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());
        Ok(())
    }
}

/// Collect `AppImageJob`s for one config: validate, run the host-once runtime
/// harvest, then build one job per matching Linux target.
#[allow(clippy::too_many_arguments)]
fn collect_config_jobs(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    cfg: &anodizer_core::config::AppImageConfig,
    dist: &Path,
    version: &str,
    project_name: &str,
    sde_epoch: Option<i64>,
    dry_run: bool,
    arch_guard: &mut ArchPathGuard,
    jobs: &mut Vec<AppImageJob>,
) -> Result<()> {
    let id = cfg.id.as_deref().unwrap_or("default").to_string();

    if let Some(ref d) = cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "appimage: render skip template")?;
        if off {
            log.verbose("appimage config skipped");
            return Ok(());
        }
    }

    validate_config_fields(cfg, &id)?;

    let os_filter: Vec<String> = cfg.os.clone().unwrap_or_else(|| vec!["linux".to_string()]);
    let binaries = collect_matching_binaries(ctx, cfg, &os_filter);
    if binaries.is_empty() {
        bail!(
            "appimage: no binaries found for config '{}' with os {:?}",
            id,
            os_filter
        );
    }

    // Host-once runtime harvest: render + run on the host-native binary, then
    // stage the populated tree at a stable dist path that every target's
    // AppImage (and an archive glob) can reuse.
    let harvested: Option<AppDirEntry> = if let Some(ref harvest) = cfg.runtime_harvest {
        let host = resolve_host_binary(&binaries).ok_or_else(|| host_missing_error(&id))?;
        let harvest_dir = dist.join(".appimage-runtime").join(&id);
        let cmd = render_harvest_command(ctx, &harvest.command, &host, &harvest_dir)?;
        if dry_run {
            log.status(&format!(
                "(dry-run) would harvest AppImage runtime via `{cmd}` → {}",
                harvest_dir.display()
            ));
        } else {
            run_harvest(&cmd, &harvest_dir, log)?;
        }
        Some(AppDirEntry {
            src: harvest_dir,
            dst: harvest.dir.trim_end_matches('/').to_string(),
        })
    } else {
        None
    };

    // Resolve the static (target-independent) config templates ONCE.
    let desktop_src = PathBuf::from(ctx.render_template(cfg.desktop.as_deref().unwrap_or(""))?);
    let icon_src = PathBuf::from(ctx.render_template(cfg.icon.as_deref().unwrap_or(""))?);
    let update_information = cfg
        .update_information
        .as_deref()
        .map(|u| ctx.render_template(u))
        .transpose()?;
    let extra_args: Vec<String> = cfg
        .extra_args
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|a| ctx.render_template(a))
        .collect::<Result<Vec<_>>>()?;
    let app_name = cfg
        .name
        .as_deref()
        .map(|n| ctx.render_template(n))
        .transpose()?
        .unwrap_or_else(|| project_name.to_string());

    let mut extra_entries: Vec<AppDirEntry> = Vec::new();
    if let Some(ref extras) = cfg.appdir_extra {
        for e in extras {
            extra_entries.push(AppDirEntry {
                src: PathBuf::from(ctx.render_template(&e.src)?),
                dst: ctx.render_template(&e.dst)?,
            });
        }
    }
    if let Some(h) = harvested {
        extra_entries.push(h);
    }

    // Group by (platform, amd64_variant) so each (os, arch) AND micro-arch
    // variant produces exactly one AppImage.
    let groups = group_by_platform(&binaries);

    for ((_, amd64_variant), group) in &groups {
        let Some(primary) = group.first() else {
            continue;
        };
        let (os, arch) = primary
            .target
            .as_deref()
            .map(anodizer_core::target::map_target)
            .unwrap_or_else(|| ("linux".to_string(), "unknown".to_string()));
        set_per_target_template_vars(
            ctx,
            primary.target.as_deref(),
            &os,
            &arch,
            amd64_variant.as_deref(),
        );

        let arch_token = primary
            .target
            .as_deref()
            .map(appimage_arch)
            .unwrap_or_else(|| arch.clone());

        let (filename, resolved_template) = resolve_appimage_filename(
            ctx,
            cfg.filename.as_deref(),
            project_name,
            version,
            &arch_token,
        )?;

        let output_path = dist.join(&filename);
        // Reject a `filename:` that renders the same `.AppImage` path for two
        // targets / amd64 variants (an override lacking `{{ .Arch }}` /
        // `{{ .Amd64 }}`): the second would silently overwrite the first.
        arch_guard.check(
            &output_path,
            "appimage",
            "image",
            &resolved_template,
            &filename,
            &primary.crate_name,
        )?;

        // Disambiguate the AppDir per amd64 variant so two non-baseline
        // variants of one platform don't stage into (and clobber) the same dir.
        let platform_subdir = match amd64_variant.as_deref() {
            Some(v) if v != "v1" => format!("{os}_{arch}_{v}"),
            _ => format!("{os}_{arch}"),
        };
        let appdir_root = dist
            .join("appimage")
            .join(&id)
            .join(platform_subdir)
            .join(format!("{app_name}.AppDir"));

        let binary_name = primary
            .metadata
            .get("binary")
            .cloned()
            .unwrap_or_else(|| primary.name.clone());

        if dry_run {
            log.status(&format!("(dry-run) would create AppImage {filename}"));
            continue;
        }

        jobs.push(AppImageJob {
            id: id.clone(),
            filename,
            app_name: app_name.clone(),
            version: version.to_string(),
            arch_token,
            update_information: update_information.clone(),
            extra_args: extra_args.clone(),
            appdir_root,
            output_path,
            binary_src: primary.path.clone(),
            binary_name,
            desktop_src: desktop_src.clone(),
            icon_src: icon_src.clone(),
            appdir_entries: extra_entries.clone(),
            primary_target: primary.target.clone(),
            primary_crate_name: primary.crate_name.clone(),
            amd64_variant: amd64_variant.clone(),
            sde_epoch,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests;

/// Environment requirements for the appimage stage: the `linuxdeploy`
/// binary whenever any `appimages:` entry is active (entries whose `skip`
/// evaluates true are inert).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = ctx.config.appimages.iter().any(|cfg| {
        !cfg.skip.as_ref().is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
    });
    if !any {
        return Vec::new();
    }
    vec![anodizer_core::EnvRequirement::Tool {
        name: "linuxdeploy".to_string(),
    }]
}
