use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{BuildConfig, BuildIgnore, BuildOverride, BuilderKind, CrossStrategy};
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_env as expand_env_vars;
use anodizer_core::stage::Stage;
use anodizer_core::target::map_target;

use super::command::{
    build_command, build_lib_command, crate_has_binary_target, detect_crate_type,
};
use super::profile::{detect_amd64_variant, detect_cargo_profile};
use super::targets::{
    DEFAULT_TARGETS, KNOWN_TARGETS, find_matching_override, is_target_ignored, resolve_target_env,
};
use super::validation::{strip_glibc_suffix, target_for_validation};
use super::workspace::{
    cargo_target_dir_with_env, check_workspace_package, ensure_targets_installed,
};

use crate::run_helpers::{
    BuildExec, BuildJob, apply_source_mutations, process_universal_binaries, run_dry_run,
    run_parallel, run_sequential, seed_determinism_state,
};

// ---------------------------------------------------------------------------
// Per-target template variables
// ---------------------------------------------------------------------------

/// Per-target template variables set before rendering a target's binary name
/// / paths and cleared afterwards so they don't leak into later targets.
/// `ArtifactExt`, `ArtifactID`, and the arch-family vars (`Arm64`/`Arm`/
/// `Amd64`/`I386`) are part of the set, so they all belong in the clear.
const PER_TARGET_VARS: &[&str] = &[
    "Target",
    "Os",
    "Arch",
    "Arm64",
    "Arm",
    "Amd64",
    "I386",
    "ArtifactExt",
    "ArtifactID",
];

/// Set the per-target template vars (`Target`/`Os`/`Arch`, the arch-family
/// var for the target's first component, `ArtifactExt`, `ArtifactID`) before
/// rendering a target's binary name and paths.
///
/// `os` is the already-mapped OS (`map_target(target).0`) so callers that
/// need it for other decisions don't re-map. The arch-family var is one of
/// `Arm64`/`Arm`/`Amd64`/`I386`, selected from the target's arch.
fn set_per_target_vars(
    vars: &mut anodizer_core::template::TemplateVars,
    target: &str,
    os: &str,
    build_id: &str,
) {
    vars.set("Target", target);
    vars.set("Os", os);
    vars.set("Arch", &map_target(target).1);
    match target.split('-').next().unwrap_or("") {
        "aarch64" => vars.set("Arm64", "v8"),
        "armv7" | "armv7l" => vars.set("Arm", "7"),
        "armv6" | "armv6l" | "arm" => vars.set("Arm", "6"),
        "x86_64" => vars.set("Amd64", "v1"),
        "i686" | "i386" | "i586" => vars.set("I386", "sse2"),
        _ => {}
    }
    vars.set("ArtifactExt", if os == "windows" { ".exe" } else { "" });
    vars.set("ArtifactID", build_id);
}

/// Clear the per-target template vars set by [`set_per_target_vars`] so they
/// don't leak into the next target's rendering.
fn clear_per_target_vars(vars: &mut anodizer_core::template::TemplateVars) {
    for k in PER_TARGET_VARS {
        vars.set(k, "");
    }
}

// ---------------------------------------------------------------------------
// BuildStage
// ---------------------------------------------------------------------------

impl Stage for super::BuildStage {
    fn name(&self) -> &str {
        "build"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("build");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;

        let parallelism = ctx.options.parallelism.max(1);

        // Collect global defaults. After the per-build settings
        // (`flags`, `ignore`, `overrides`) live under `defaults.builds.*`
        // rather than flat on `defaults`, mirroring `BuildConfig`'s shape.
        let defaults = ctx.config.defaults.as_ref();
        let default_builds = defaults.and_then(|d| d.builds.as_ref());
        let default_targets: Vec<String> = defaults
            .and_then(|d| d.targets.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| DEFAULT_TARGETS.iter().map(|s| (*s).to_string()).collect());
        let default_strategy = defaults
            .and_then(|d| d.cross.clone())
            .unwrap_or(CrossStrategy::Auto);
        let default_flags: Option<Vec<String>> = default_builds.and_then(|b| b.flags.clone());
        let default_ignores: Vec<BuildIgnore> = default_builds
            .and_then(|b| b.ignore.clone())
            .unwrap_or_default();
        let default_overrides: Vec<BuildOverride> = default_builds
            .and_then(|b| b.overrides.clone())
            .unwrap_or_default();

        // Collect crates to process (cloned to avoid borrow conflict with ctx.artifacts)
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        apply_source_mutations(ctx, &crates, &default_targets, dry_run, &log)?;

        // -----------------------------------------------------------------
        // Flatten the nested (crate, build, target) loops into a list of
        // BuildJob descriptors. No compilation happens here.
        // -----------------------------------------------------------------

        let commit_timestamp = ctx
            .template_vars()
            .get("CommitTimestamp")
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        seed_determinism_state(ctx, &commit_timestamp, &log)?;

        let inputs = PlanInputs {
            crates: &crates,
            default_targets: &default_targets,
            default_strategy: &default_strategy,
            default_flags: &default_flags,
            default_ignores: &default_ignores,
            default_overrides: &default_overrides,
            commit_timestamp: &commit_timestamp,
        };
        let (build_jobs, copy_jobs) = plan_build_jobs(ctx, &log, &inputs)?;

        // Record which crates actually received an in-scope build/copy job so
        // the binary-artifact guard can tell "no in-scope target in this
        // shard" (skip) from "built but produced no binary" (real mis-scope).
        // A crate filtered out by `--targets` / `build.ignore` lands here with
        // zero jobs and is therefore absent from this set.
        let built_crate_names: std::collections::HashSet<String> = build_jobs
            .iter()
            .chain(copy_jobs.iter())
            .map(|j| j.crate_name.clone())
            .collect();
        ctx.set_built_crate_names(built_crate_names);

        // -----------------------------------------------------------------
        // Ensure cross-compilation targets are installed via rustup.
        // -----------------------------------------------------------------

        {
            let unique_targets: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                build_jobs
                    .iter()
                    .filter_map(|j| {
                        if seen.insert(j.target.clone()) {
                            Some(j.target.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            ensure_targets_installed(ctx, &unique_targets, &log, dry_run)?;
        }

        // -----------------------------------------------------------------
        // Execute build jobs (with parallelism) then copy_from jobs.
        // -----------------------------------------------------------------

        // Rust builds sharing the same workspace target/ directory can deadlock
        // when multiple cargo invocations run in parallel (they contend on
        // target/ directory locks). Force sequential execution unless the user
        // has only a single build job.
        let effective_parallelism = if build_jobs.len() > 1 { 1 } else { parallelism };

        let template_vars = ctx.template_vars().clone();
        let dist_dir = ctx.config.dist.clone();

        let exec = BuildExec {
            log: &log,
            template_vars: &template_vars,
            dist_dir: &dist_dir,
            dry_run,
            commit_timestamp: &commit_timestamp,
        };

        if dry_run {
            run_dry_run(ctx, &exec, &build_jobs, &copy_jobs)?;
        } else if effective_parallelism <= 1 || build_jobs.len() <= 1 {
            run_sequential(ctx, &exec, &build_jobs, &copy_jobs)?;
        } else {
            run_parallel(ctx, &exec, &build_jobs, &copy_jobs, effective_parallelism)?;
        }

        process_universal_binaries(ctx, &crates, dry_run)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Build planning
// ---------------------------------------------------------------------------

struct PlanInputs<'a> {
    crates: &'a [anodizer_core::config::CrateConfig],
    default_targets: &'a [String],
    default_strategy: &'a CrossStrategy,
    default_flags: &'a Option<Vec<String>>,
    default_ignores: &'a [BuildIgnore],
    default_overrides: &'a [BuildOverride],
    commit_timestamp: &'a str,
}

/// Merge reproducibility RUSTFLAGS for a build whose working directory is
/// `cwd`, without clobbering externally-set flags.
///
/// `config` (a per-target `build.env` RUSTFLAGS) wins when present;
/// otherwise fall back to `inherited` — the process env, where the
/// determinism harness places the Windows-MSVC reproducibility flags
/// (`/Brepro`, `/DEBUG:NONE`, `codegen-units=1`, ...) alongside its own
/// `--remap-path-prefix` rules. A `--remap-path-prefix=<cwd>=/build` rule
/// is appended so source paths normalize, UNLESS the chosen base already
/// remaps `cwd` (the harness remaps the worktree, which is `cwd`) — a
/// second rule for the same prefix is shadowed by rustc's first-match-wins
/// and would only mislead.
///
/// The cargo build inherits the process env (`Command::envs` adds, does
/// not clear). Overwriting RUSTFLAGS here with only the remap rule would —
/// per cargo's `RUSTFLAGS` over `CARGO_TARGET_<triple>_RUSTFLAGS`
/// precedence — suppress the harness-injected flags and reintroduce the PE
/// `TimeDateStamp` drift on Windows. Blank (whitespace-only) values are
/// treated as unset.
fn merge_reproducible_rustflags(
    config: Option<&str>,
    inherited: Option<&str>,
    cwd: &str,
) -> String {
    let base = config
        .filter(|s| !s.trim().is_empty())
        .or(inherited.filter(|s| !s.trim().is_empty()))
        .map(str::trim)
        .unwrap_or("");
    if base.is_empty() {
        format!("--remap-path-prefix={cwd}=/build")
    } else if base.contains(&format!("--remap-path-prefix={cwd}=")) {
        base.to_string()
    } else {
        format!("{base} --remap-path-prefix={cwd}=/build")
    }
}

/// Flatten the nested (crate, build, target) tree into a list of
/// `BuildJob` descriptors. No compilation happens here — the planner
/// only resolves overrides, renders templates, and assembles the
/// `BuildCommand`s the executor will spawn.
///
/// Returns `(build_jobs, copy_jobs)`; the latter cover `copy_from:` jobs
/// that wait for their source build to land before being copied.
fn plan_build_jobs(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    inputs: &PlanInputs<'_>,
) -> Result<(Vec<BuildJob>, Vec<BuildJob>)> {
    // Re-bind PlanInputs fields as owned/slice locals so the
    // planning expressions below can reference them by short name.
    let default_targets: Vec<String> = inputs.default_targets.to_vec();
    let default_strategy: CrossStrategy = inputs.default_strategy.clone();
    let default_flags: Option<Vec<String>> = inputs.default_flags.clone();
    let default_ignores: Vec<BuildIgnore> = inputs.default_ignores.to_vec();
    let default_overrides: Vec<BuildOverride> = inputs.default_overrides.to_vec();
    let commit_timestamp: &str = inputs.commit_timestamp;
    let crates = inputs.crates;

    let mut build_jobs: Vec<BuildJob> = Vec::new();
    let mut copy_jobs: Vec<BuildJob> = Vec::new();

    for crate_cfg in crates {
        // Determine builds for this crate
        let builds: Vec<BuildConfig> = match &crate_cfg.builds {
            Some(b) if !b.is_empty() => b.clone(),
            _ => {
                // No builds configured — only create a default binary build if
                // the crate actually has a binary target (src/main.rs or [[bin]]).
                // Library-only crates should not get a default --bin build.
                if crate_has_binary_target(&crate_cfg.path) {
                    vec![BuildConfig {
                        binary: Some(crate_cfg.name.clone()),
                        ..Default::default()
                    }]
                } else {
                    log.status(&format!(
                        "skipping crate '{}' — no builds configured and no binary target found",
                        crate_cfg.name
                    ));
                    continue;
                }
            }
        };

        // Validate: no duplicate build IDs within this crate
        {
            let mut seen_ids: HashSet<&str> = HashSet::new();
            for build in &builds {
                if let Some(ref id) = build.id
                    && !seen_ids.insert(id.as_str())
                {
                    anyhow::bail!(
                        "found 2 builds with the ID '{}' in crate '{}'",
                        id,
                        crate_cfg.name
                    );
                }
            }
        }

        // Detect crate type for cdylib/wasm awareness (once per crate)
        let crate_type = detect_crate_type(&crate_cfg.path);
        let is_wasm_crate = matches!(crate_type.as_deref(), Some("cdylib"));
        let is_library = matches!(
            crate_type.as_deref(),
            Some("cdylib" | "staticlib" | "dylib")
        );

        for build in &builds {
            // `builder: prebuilt` imports a binary the operator staged on
            // disk instead of running `cargo build`. The cargo-targeted
            // checks below (binary-target detection, workspace `--package`
            // validation, cross-tool resolution, env cascading) all skip
            // when the planner is just importing bytes; the prebuilt
            // helper renders `prebuilt.path` per target, stat()s it, and
            // registers an `ArtifactKind::Binary` directly.
            if matches!(build.builder, Some(BuilderKind::Prebuilt)) {
                plan_prebuilt_build(ctx, log, crate_cfg, build, inputs)?;
                continue;
            }

            // If this build has no explicit `binary:` and the crate has
            // no binary target on disk (no `src/main.rs`, no `[[bin]]`),
            // skip it. This protects library-only crates that inherited
            // a `defaults.builds:` template — the template's missing
            // `binary:` field would otherwise fall back to the crate
            // name and `cargo build --bin <library-name>` would fail.
            if build.binary.is_none() && !crate_has_binary_target(&crate_cfg.path) {
                log.status(&format!(
                    "skipping build for crate '{}' — no explicit binary, no binary target found",
                    crate_cfg.name
                ));
                continue;
            }

            // Resolve binary name template — falls back to the crate's
            // `name` field when not set so `defaults.builds` (the
            // path-mirrored template) can omit it.
            let binary_field: String = build
                .binary
                .clone()
                .unwrap_or_else(|| crate_cfg.name.clone());
            // Skip builds marked with skip: true/template
            let should_skip = match build.skip.as_ref() {
                Some(s) => s
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| {
                        format!(
                            "build: render skip template for build '{}'",
                            build.id.as_deref().unwrap_or(&binary_field)
                        )
                    })?,
                None => false,
            };
            if should_skip {
                log.status(&format!(
                    "skipping build '{}' (skip: true)",
                    build.id.as_deref().unwrap_or(&binary_field)
                ));
                continue;
            }

            // NOTE: Binary name rendering is deferred to the per-target loop
            // below so that per-target template variables (Os, Arch, Target)
            // are available in the template. The raw template is used in log
            // messages before the target loop.
            let binary_name_raw = binary_field.as_str();

            // Targets: per-build override (even if empty), else global defaults.
            // An explicitly empty list (Some(vec![])) means "skip this build".
            // Only None (not specified) falls through to defaults.
            let mut targets: Vec<String> = if build.targets.is_some() {
                build.targets.clone().unwrap_or_default()
            } else if !default_targets.is_empty() {
                default_targets.clone()
            } else {
                Vec::new()
            };

            // --single-target: filter targets to only the specified triple.
            //
            // Step 1 — exact match. Step 2 — fall back to a best-effort
            // (os, arch) alias-table match via
            // `anodizer_core::partial::find_runtime_target` so a config
            // listing `x86_64-apple-darwin` still picks up a host triple
            // spelled differently by `rustc -vV` via the OS / arch alias
            // tables. When the
            // user explicitly requested `--single-target` and zero
            // configured targets match (even after alias resolution),
            // this is an error: silent "warn then skip" produced empty
            // `dist/` directories in CI that exited 0.
            if let Some(ref single) = ctx.options.single_target {
                let had_targets = !targets.is_empty();
                if had_targets {
                    let original = targets.clone();
                    targets.retain(|t| t == single);
                    if targets.is_empty()
                        && let Some(matched) =
                            anodizer_core::partial::find_runtime_target(single, &original)
                    {
                        log.verbose(&format!(
                            "host '{}' matched configured target '{}' via alias table (--single-target)",
                            single, matched
                        ));
                        targets.push(matched);
                    }
                    if targets.is_empty() {
                        anyhow::bail!(
                            "--single-target: host triple '{}' is not in configured targets for {}/{} \
                             (configured: [{}]). Set TARGET=<triple> or update build.targets to include the host.",
                            single,
                            crate_cfg.name,
                            binary_name_raw,
                            original.join(", ")
                        );
                    }
                }
            }

            // --split: filter targets to those matching the partial target
            if let Some(ref partial) = ctx.options.partial_target {
                let had_targets = !targets.is_empty();
                targets = partial.filter_targets(&targets);
                if had_targets && targets.is_empty() {
                    log.verbose(&format!(
                        "no targets match partial filter for {}/{}, skipping",
                        crate_cfg.name, binary_name_raw
                    ));
                    continue;
                }
            }

            // If no targets configured, skip (caller should ensure defaults)
            if targets.is_empty() {
                log.warn(&format!(
                    "no targets configured for {}/{}, skipping",
                    crate_cfg.name, binary_name_raw
                ));
                continue;
            }

            // Validate targets against the known list (unknown target is an error)
            for target in &targets {
                let validation_target = target_for_validation(target);
                if !KNOWN_TARGETS.contains(&validation_target) {
                    anyhow::bail!(
                        "target '{}' is not in the known targets list and may be invalid; \
                         if this is a custom target, add it to your build config",
                        target
                    );
                }
            }

            // Strategy: per-crate override, else global default
            let strategy = crate_cfg
                .cross
                .clone()
                .unwrap_or_else(|| default_strategy.clone());

            // Flags: per-build, else global default, else `["--release"]`.
            // Default to `--release` for production builds. Users can
            // explicitly set `flags: []` (empty list) in their config to
            // get a debug build. `Some(vec![])` is distinct from `None`,
            // so the fallback to `["--release"]` only fires when neither
            // a per-build nor a default-builds flags list is configured.
            let flags: Vec<String> = build
                .flags
                .clone()
                .or_else(|| default_flags.clone())
                .unwrap_or_else(|| vec!["--release".to_string()]);

            // Features and no_default_features
            let features: Vec<String> = build.features.clone().unwrap_or_default();
            let no_default_features: bool = build.no_default_features.unwrap_or(false);

            // Per-build ignore/overrides, falling back to defaults
            let build_ignores: Vec<BuildIgnore> = build
                .ignore
                .clone()
                .unwrap_or_else(|| default_ignores.clone());
            let build_overrides: Vec<BuildOverride> = build
                .overrides
                .clone()
                .unwrap_or_else(|| default_overrides.clone());

            // Cross tool override — takes precedence over the `cross` strategy
            let cross_tool = build.cross_tool.clone();
            if cross_tool.is_some() && crate_cfg.cross.is_some() {
                log.warn(
                    "both `cross` strategy and `cross_tool` are set; `cross_tool` takes precedence",
                );
            }

            // Command override (e.g. "auditable build" for `cargo auditable build`)
            let command_override = build.command.clone();

            // Workspace --package validation: if building from a workspace root,
            // ensure --package is specified in the build flags.
            check_workspace_package(&crate_cfg.path, &flags)?;

            // Resolve no_unique_dist_dir: per-build overrides crate-level
            let no_unique_dist_dir_val = if let Some(s) = build.no_unique_dist_dir.as_ref() {
                s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "build: render no_unique_dist_dir template")?
            } else if let Some(s) = crate_cfg.no_unique_dist_dir.as_ref() {
                s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "crate: render no_unique_dist_dir template")?
            } else {
                false
            };

            // Per-target env (target-keyed map in BuildConfig.env)
            for target in &targets {
                // Check ignore list
                if is_target_ignored(target, &build_ignores) {
                    log.verbose(&format!("ignoring target {} (matched ignore rule)", target));
                    continue;
                }

                // Apply overrides: merge env, append flags, extend features
                let matched_override =
                    find_matching_override(target, &build_overrides, log, ctx.is_strict())?;
                let mut effective_flags: Vec<String> = flags.clone();
                if let Some(ov) = matched_override
                    && let Some(extra) = &ov.flags
                {
                    effective_flags.extend(extra.iter().cloned());
                }
                let effective_features: Vec<String> = if let Some(ov) = matched_override {
                    let mut f = features.clone();
                    if let Some(ref extra) = ov.features {
                        f.extend(extra.iter().cloned());
                    }
                    f
                } else {
                    features.clone()
                };

                // Template-render each flag entry. Flags survive as
                // discrete argv tokens (no whitespace splitting), so
                // quoted values like `--cfg=feature="foo bar"` round-trip
                // correctly. Entries that render to an empty string are
                // dropped — equivalent to the prior `flags: ""` -> debug
                // escape hatch but per-entry instead of whole-string.
                let effective_flags: Vec<String> = effective_flags
                    .into_iter()
                    .map(|f| {
                        ctx.render_template(&f)
                            .with_context(|| format!("build: render flags template '{f}'"))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect();

                // Determine the binary path
                // Flags may contain --release, --profile release, or
                // --profile=<name>; detect the effective cargo profile.
                let profile = detect_cargo_profile(&effective_flags);

                let is_wasm_target = target.contains("wasm32");
                let (os, _arch) = map_target(target);

                // Set per-target template vars BEFORE rendering binary name
                set_per_target_vars(
                    ctx.template_vars_mut(),
                    target,
                    &os,
                    build.id.as_deref().unwrap_or(""),
                );

                // Render binary name with per-target template vars available
                let binary_name = ctx.render_template(binary_name_raw).unwrap_or_else(|e| {
                    log.warn(&format!(
                        "failed to render binary template '{}': {}, using raw value",
                        binary_name_raw, e
                    ));
                    binary_name_raw.to_string()
                });

                // Determine the output file name based on target and crate type
                let (output_name, artifact_kind) = if is_wasm_target && is_wasm_crate {
                    // wasm32 target with cdylib — output is .wasm
                    (format!("{}.wasm", binary_name), ArtifactKind::Wasm)
                } else if is_library && !is_wasm_target {
                    // Library target — output is .so/.dylib/.dll
                    let ext = match os.as_str() {
                        "windows" => "dll",
                        "darwin" => "dylib",
                        _ => "so",
                    };
                    let prefix = if os == "windows" { "" } else { "lib" };
                    (
                        format!("{}{}.{}", prefix, binary_name, ext),
                        ArtifactKind::Library,
                    )
                } else {
                    // Standard binary
                    let name = if os == "windows" {
                        format!("{}.exe", binary_name)
                    } else {
                        binary_name.clone()
                    };
                    (name, ArtifactKind::Binary)
                };

                // Strip glibc version suffix for the cargo target dir path.
                // e.g. "aarch64-unknown-linux-gnu.2.17" -> stripped for dir
                let (cargo_target_name, _has_glibc_suffix) = strip_glibc_suffix(target);

                // Glob-match per-target env keys (C-new-2). Owned merged
                // map; cargo_target_dir takes a borrow of it.
                let raw_target_env: Option<HashMap<String, String>> =
                    resolve_target_env(build.env.as_ref(), target, log, ctx.is_strict())?;

                // Use stripped target name for directory path
                let bin_path = cargo_target_dir_with_env(raw_target_env.as_ref(), ctx.env_source())
                    .join(cargo_target_name)
                    .join(profile)
                    .join(&output_name);

                // Handle copy_from: skip compilation, queue for after builds
                if let Some(src_binary) = &build.copy_from {
                    let src_name = if os == "windows" {
                        format!("{}.exe", src_binary)
                    } else {
                        src_binary.clone()
                    };
                    let src_path =
                        cargo_target_dir_with_env(raw_target_env.as_ref(), ctx.env_source())
                            .join(cargo_target_name)
                            .join(profile)
                            .join(&src_name);

                    // Clear per-target template vars before continuing
                    clear_per_target_vars(ctx.template_vars_mut());

                    let copy_variant = raw_target_env
                        .as_ref()
                        .and_then(|e| detect_amd64_variant(target, e));
                    copy_jobs.push(BuildJob {
                        cmd: None,
                        copy_from: Some((src_path, bin_path.clone())),
                        bin_path,
                        artifact_kind,
                        target: target.clone(),
                        crate_name: crate_cfg.name.clone(),
                        binary_name: binary_name.clone(),
                        build_id: build.id.clone(),
                        reproducible: false,
                        pre_hooks: Vec::new(),
                        post_hooks: Vec::new(),
                        no_unique_dist_dir: no_unique_dist_dir_val,
                        crate_path: crate_cfg.path.clone(),
                        mod_timestamp: build.mod_timestamp.clone(),
                        amd64_variant: copy_variant,
                        // copy_from jobs run no pre/post hooks (empty above), so
                        // this is inert. It carries the RAW (un-rendered)
                        // per-target env — the rendered `target_env` (with
                        // RUSTFLAGS/SOURCE_DATE_EPOCH + override merges folded
                        // in) is built only on the compile path below. Anyone
                        // later attaching hooks here must switch to that
                        // rendered map, not this pre-render one.
                        build_env: raw_target_env.clone().unwrap_or_default(),
                    });
                    continue;
                }

                // No copy_from: build a compilation command. Use the
                // glob-matched env (C-new-2) — same merged map as above.
                let mut target_env: HashMap<String, String> =
                    raw_target_env.clone().unwrap_or_default();

                // Render env values and expand shell-style env var references.
                // Cascade: each rendered KEY is injected into the template
                // context's env map BEFORE rendering later entries so that
                // `{{ .Env.KEY }}` references resolve to the same-block value.
                // Iteration is sorted for deterministic order; full
                // user-insertion-order cascade requires changing the YAML
                // schema to an ordered list — tracked upstream.
                let mut rendered_env: HashMap<String, String> = HashMap::new();
                let mut keys: Vec<&String> = target_env.keys().collect();
                keys.sort();
                for k in keys {
                    let v = &target_env[k];
                    let rendered_val = ctx.render_template(v).unwrap_or_else(|e| {
                        log.warn(&format!(
                            "failed to render env value for '{}': {}, using raw value",
                            k, e
                        ));
                        v.clone()
                    });
                    let expanded = expand_env_vars(&rendered_val);
                    // Inject into ctx env so later entries (and templated
                    // fields) see this KEY via `{{ .Env.KEY }}`.
                    ctx.template_vars_mut().set_env(k, &expanded);
                    rendered_env.insert(k.clone(), expanded);
                }
                target_env = rendered_env;

                // Merge override env if matched
                if let Some(ov) = matched_override
                    && let Some(ref ov_env) = ov.env
                {
                    let parsed = anodizer_core::config::parse_env_entries(ov_env)
                        .with_context(|| "build override: parse env entries")?;
                    for (k, v) in &parsed {
                        let rendered_val = ctx.render_template(v).unwrap_or_else(|e| {
                            log.warn(&format!(
                                "failed to render override env value for '{}': {}, using raw value",
                                k, e
                            ));
                            v.clone()
                        });
                        target_env.insert(k.clone(), expand_env_vars(&rendered_val));
                    }
                }

                // Set per-target hook context: Name, Path, Ext
                ctx.template_vars_mut().set("Name", &binary_name);
                ctx.template_vars_mut()
                    .set("Path", &bin_path.to_string_lossy());
                ctx.template_vars_mut()
                    .set("Ext", if os == "windows" { ".exe" } else { "" });

                // Remove per-target template variables to avoid leaking
                clear_per_target_vars(ctx.template_vars_mut());
                // Name/Path/Ext are set just above for the hook context only
                // on the compile path, so they're cleared here (not part of
                // the shared per-target set).
                ctx.template_vars_mut().set("Name", "");
                ctx.template_vars_mut().set("Path", "");
                ctx.template_vars_mut().set("Ext", "");

                // Reproducible builds: inject SOURCE_DATE_EPOCH and RUSTFLAGS
                if build.reproducible.unwrap_or(false) {
                    target_env
                        .entry("SOURCE_DATE_EPOCH".to_string())
                        .or_insert_with(|| commit_timestamp.to_string());

                    let cwd = std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .to_string_lossy()
                        .into_owned();
                    let inherited_rustflags = ctx.env_source().var("RUSTFLAGS");
                    let new_rustflags = merge_reproducible_rustflags(
                        target_env.get("RUSTFLAGS").map(String::as_str),
                        inherited_rustflags.as_deref(),
                        &cwd,
                    );
                    target_env.insert("RUSTFLAGS".to_string(), new_rustflags);
                }

                let build_ctx = crate::command::BuildContext {
                    crate_path: &crate_cfg.path,
                    target,
                    strategy: &strategy,
                    flags: &effective_flags,
                    features: &effective_features,
                    no_default_features,
                    env: &target_env,
                    cross_tool: cross_tool.as_deref(),
                    command_override: command_override.as_deref(),
                };

                // For library/wasm targets, use --lib; otherwise --bin
                let cmd = if is_library || is_wasm_target {
                    build_lib_command(&build_ctx)
                } else {
                    build_command(&binary_name, &build_ctx)
                };

                build_jobs.push(BuildJob {
                    cmd: Some(cmd),
                    copy_from: None,
                    bin_path,
                    artifact_kind,
                    target: target.clone(),
                    crate_name: crate_cfg.name.clone(),
                    binary_name: binary_name.clone(),
                    build_id: build.id.clone(),
                    reproducible: build.reproducible.unwrap_or(false),
                    pre_hooks: build
                        .hooks
                        .as_ref()
                        .and_then(|h| h.pre.clone())
                        .unwrap_or_default(),
                    post_hooks: build
                        .hooks
                        .as_ref()
                        .and_then(|h| h.post.clone())
                        .unwrap_or_default(),
                    no_unique_dist_dir: no_unique_dist_dir_val,
                    crate_path: crate_cfg.path.clone(),
                    mod_timestamp: build.mod_timestamp.clone(),
                    amd64_variant: detect_amd64_variant(target, &target_env),
                    // Fully-rendered per-target build env (overrides + the
                    // reproducible RUSTFLAGS/SOURCE_DATE_EPOCH merges already
                    // folded in) flows into this job's build hooks beneath the
                    // hook's own env:, which takes precedence.
                    build_env: target_env.clone(),
                });
            }
        }
    }

    Ok((build_jobs, copy_jobs))
}

/// Plan a single `builder: prebuilt` build by rendering its
/// `prebuilt.path` template per target, stat()-ing the rendered path,
/// and registering an `ArtifactKind::Binary` directly in `ctx.artifacts`.
///
/// No `BuildJob` is emitted — the cargo runner has nothing to do for an
/// imported binary. Hooks (`pre`/`post`), `skip:`, target filters
/// (`--single-target`, `--split`, `ignore`), and the per-target
/// template-var lifecycle (Os, Arch, Target, Amd64, ArtifactExt,
/// ArtifactID) are all honoured the same way as the cargo path so
/// downstream stages see a uniform artifact shape regardless of which
/// builder produced the bytes.
///
/// Cargo-only knobs (`features`, `no_default_features`, `command`,
/// `cross_tool`, `flags`, `reproducible`) are rejected at config-load
/// time by [`anodizer_core::config::validate_builds`]; the planner can
/// therefore assume the build entry is well-formed by the time it gets
/// here. `targets:` is also required-explicit by that validator.
fn plan_prebuilt_build(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    build: &BuildConfig,
    inputs: &PlanInputs<'_>,
) -> Result<()> {
    let binary_field: String = build
        .binary
        .clone()
        .unwrap_or_else(|| crate_cfg.name.clone());

    let should_skip = match build.skip.as_ref() {
        Some(s) => s
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| {
                format!(
                    "build: render skip template for prebuilt build '{}'",
                    build.id.as_deref().unwrap_or(&binary_field)
                )
            })?,
        None => false,
    };
    if should_skip {
        log.status(&format!(
            "skipping prebuilt build '{}' (skip: true)",
            build.id.as_deref().unwrap_or(&binary_field)
        ));
        return Ok(());
    }

    let prebuilt = build.prebuilt.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "internal: prebuilt build '{}' reached the planner without a `prebuilt:` block \
             (validate_builds should have rejected this at config-load)",
            build.id.as_deref().unwrap_or(&binary_field)
        )
    })?;
    let path_template = prebuilt.path.clone();

    // `targets:` is required-explicit for prebuilt builds (enforced by
    // validate_builds). Honour `--single-target` / `--split` the same
    // way the cargo path does so operators can shard prebuilt imports.
    let mut targets: Vec<String> = build.targets.clone().unwrap_or_default();
    if let Some(ref single) = ctx.options.single_target {
        let original = targets.clone();
        targets.retain(|t| t == single);
        if targets.is_empty()
            && let Some(matched) = anodizer_core::partial::find_runtime_target(single, &original)
        {
            log.verbose(&format!(
                "host '{}' matched configured prebuilt target '{}' via alias table (--single-target)",
                single, matched
            ));
            targets.push(matched);
        }
        if targets.is_empty() {
            anyhow::bail!(
                "--single-target: host triple '{}' is not in configured prebuilt targets for {}/{} \
                 (configured: [{}]).",
                single,
                crate_cfg.name,
                binary_field,
                original.join(", ")
            );
        }
    }
    if let Some(ref partial) = ctx.options.partial_target {
        targets = partial.filter_targets(&targets);
        if targets.is_empty() {
            log.verbose(&format!(
                "no prebuilt targets match partial filter for {}/{}, skipping",
                crate_cfg.name, binary_field
            ));
            return Ok(());
        }
    }

    let build_ignores: Vec<BuildIgnore> = build
        .ignore
        .clone()
        .unwrap_or_else(|| inputs.default_ignores.to_vec());

    for target in &targets {
        if is_target_ignored(target, &build_ignores) {
            log.verbose(&format!(
                "ignoring prebuilt target {} (matched ignore rule)",
                target
            ));
            continue;
        }

        let (os, _arch) = map_target(target);

        set_per_target_vars(
            ctx.template_vars_mut(),
            target,
            &os,
            build.id.as_deref().unwrap_or(""),
        );
        let first_component = target.split('-').next().unwrap_or("");

        let binary_name = ctx.render_template(&binary_field).unwrap_or_else(|e| {
            log.warn(&format!(
                "failed to render binary template '{}': {}, using raw value",
                binary_field, e
            ));
            binary_field.clone()
        });

        let rendered_path = ctx.render_template(&path_template).with_context(|| {
            format!(
                "build: render prebuilt.path template '{}' for target {}",
                path_template, target
            )
        })?;

        clear_per_target_vars(ctx.template_vars_mut());

        let staged_path = std::path::PathBuf::from(&rendered_path);
        let dry_run = ctx.options.dry_run;
        if !dry_run {
            std::fs::metadata(&staged_path).with_context(|| {
                format!(
                    "prebuilt: failed to stat imported binary at '{}' (rendered from \
                     `prebuilt.path: {}`) for target '{}'. Stage the binary before running \
                     `anodize build`, or check the path template renders to a real file.",
                    rendered_path, path_template, target
                )
            })?;
        }

        let amd64_variant = if first_component == "x86_64" {
            Some("v1".to_string())
        } else {
            None
        };

        let dist_dir = ctx.config.dist.clone();
        crate::run_helpers::add_artifact(
            ctx,
            &dist_dir,
            dry_run,
            &staged_path,
            ArtifactKind::Binary,
            target,
            &crate_cfg.name,
            &binary_name,
            &build.id,
            false,
            &amd64_variant,
        )?;

        if dry_run {
            log.status(&format!(
                "(dry-run) would import prebuilt {}/{} ({}) from {}",
                crate_cfg.name,
                binary_name,
                target,
                staged_path.display()
            ));
        } else {
            log.status(&format!(
                "imported prebuilt {}/{} ({}) from {}",
                crate_cfg.name,
                binary_name,
                target,
                staged_path.display()
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod reproducible_rustflags_tests {
    use super::merge_reproducible_rustflags;

    const CWD: &str = "/work";
    const REMAP: &str = "--remap-path-prefix=/work=/build";

    #[test]
    fn preserves_inherited_msvc_flags_from_harness() {
        // The determinism harness injects /Brepro into the child's process
        // RUSTFLAGS. With no per-target config override, the build stage must
        // carry it through — clobbering it reintroduces the PE timestamp drift.
        let inherited = "-C link-arg=/Brepro -C link-arg=/DEBUG:NONE";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD);
        assert!(merged.contains("/Brepro"), "got {merged}");
        assert!(merged.contains("/DEBUG:NONE"), "got {merged}");
        assert!(merged.ends_with(REMAP), "remap must be appended: {merged}");
    }

    #[test]
    fn config_override_wins_over_inherited() {
        let merged = merge_reproducible_rustflags(
            Some("-C target-cpu=native"),
            Some("-C link-arg=/Brepro"),
            CWD,
        );
        assert_eq!(merged, format!("-C target-cpu=native {REMAP}"));
    }

    #[test]
    fn remap_only_when_nothing_inherited() {
        assert_eq!(merge_reproducible_rustflags(None, None, CWD), REMAP);
        // Blank (whitespace-only) values are treated as unset, not as a real
        // base to append to — no leading-space artifact.
        assert_eq!(
            merge_reproducible_rustflags(Some(""), Some("  "), CWD),
            REMAP
        );
    }

    #[test]
    fn does_not_double_remap_when_cwd_already_remapped() {
        // The harness already remaps the worktree (== cwd) to /anodize.
        // A second rule for the same prefix is shadowed (rustc first-match-
        // wins) and only misleads, so it must not be appended.
        let inherited = "-C link-arg=/Brepro --remap-path-prefix=/work=/anodize";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD);
        assert_eq!(merged, inherited, "must not append a shadowed cwd remap");
        assert_eq!(
            merged.matches("--remap-path-prefix=/work=").count(),
            1,
            "exactly one remap rule for the cwd prefix: {merged}"
        );
    }
}

#[cfg(test)]
mod per_target_var_tests {
    use super::{PER_TARGET_VARS, clear_per_target_vars, set_per_target_vars};
    use anodizer_core::template::TemplateVars;

    fn vars_for(target: &str, os: &str, id: &str) -> TemplateVars {
        let mut v = TemplateVars::new();
        set_per_target_vars(&mut v, target, os, id);
        v
    }

    #[test]
    fn sets_arm64_for_aarch64() {
        let v = vars_for("aarch64-unknown-linux-gnu", "linux", "");
        assert_eq!(
            v.get("Target").map(String::as_str),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(v.get("Os").map(String::as_str), Some("linux"));
        assert_eq!(v.get("Arch").map(String::as_str), Some("arm64"));
        assert_eq!(v.get("Arm64").map(String::as_str), Some("v8"));
        assert_eq!(v.get("ArtifactExt").map(String::as_str), Some(""));
    }

    #[test]
    fn sets_amd64_and_windows_ext() {
        let v = vars_for("x86_64-pc-windows-msvc", "windows", "cli");
        assert_eq!(v.get("Amd64").map(String::as_str), Some("v1"));
        assert_eq!(v.get("ArtifactExt").map(String::as_str), Some(".exe"));
        assert_eq!(v.get("ArtifactID").map(String::as_str), Some("cli"));
    }

    #[test]
    fn arm_and_i386_variants() {
        assert_eq!(
            vars_for("armv7-unknown-linux-gnueabihf", "linux", "")
                .get("Arm")
                .map(String::as_str),
            Some("7")
        );
        assert_eq!(
            vars_for("i686-unknown-linux-gnu", "linux", "")
                .get("I386")
                .map(String::as_str),
            Some("sse2")
        );
    }

    #[test]
    fn clear_empties_every_set_var() {
        let mut v = vars_for("x86_64-pc-windows-msvc", "windows", "cli");
        clear_per_target_vars(&mut v);
        for k in PER_TARGET_VARS {
            assert_eq!(
                v.get(k).map(String::as_str),
                Some(""),
                "{k} must be cleared"
            );
        }
    }
}
