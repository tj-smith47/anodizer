use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{BuildConfig, BuildIgnore, BuildOverride, BuilderKind, CrossStrategy};
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_env as expand_env_vars;
use anodizer_core::stage::Stage;
use anodizer_core::target::map_target;

use anodizer_core::build_plan::{crate_declares_bin, planned_builds};

use super::command::{
    build_command, build_lib_command, crate_has_binary_target, detect_crate_type,
};
use super::profile::detect_cargo_profile;
use super::targets::{
    KNOWN_TARGETS, find_matching_override, is_target_ignored, resolve_target_env,
};
use super::validation::{strip_glibc_suffix, target_for_validation};
use super::workspace::{
    cargo_target_dir_with_env, check_workspace_package, ensure_targets_installed,
};

use crate::run_helpers::{
    BuildExec, BuildJob, apply_source_mutations, process_universal_binaries, run_dry_run,
    run_parallel, run_sequential, seed_determinism_state,
};

// Per-target template variable seeding lives in core
// (`anodizer_core::build_env::seed_build_target_vars`) so the config-time
// env projection seeds the exact set this planner renders with.
use anodizer_core::build_env::{
    build_amd64_variant, clear_build_target_vars, prebuilt_amd64_variant, seed_build_target_vars,
};

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
        let default_targets: Vec<String> = ctx.config.effective_default_targets();
        let default_strategy = ctx.config.default_cross_strategy();
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
            .crate_universe()
            .into_iter()
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

/// Merge reproducibility RUSTFLAGS for a build of `target` whose working
/// directory is `cwd`, without clobbering externally-set flags.
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
/// When `target` is a `*-pc-windows-msvc` triple, the
/// [`MSVC_DETERMINISM_RUSTFLAGS`](anodizer_core::determinism::MSVC_DETERMINISM_RUSTFLAGS)
/// set is merged in (deduplicating any token already present from config or
/// the inherited env). This is keyed on the TARGET triple, not the host, so
/// a Windows binary cross-built from Linux is reproducible too. Without it,
/// a consumer's `reproducible: true` Windows build would still stamp the PE
/// COFF `TimeDateStamp` (offset 0x108) with wall-clock time and drift.
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
    target: &str,
) -> String {
    let base = config
        .filter(|s| !s.trim().is_empty())
        .or(inherited.filter(|s| !s.trim().is_empty()))
        .map(str::trim)
        .unwrap_or("");
    let with_remap = if base.is_empty() {
        format!("--remap-path-prefix={cwd}=/build")
    } else if base.contains(&format!("--remap-path-prefix={cwd}=")) {
        base.to_string()
    } else {
        format!("{base} --remap-path-prefix={cwd}=/build")
    };
    if anodizer_core::target::is_windows_msvc(target) {
        anodizer_core::determinism::merge_msvc_determinism_rustflags(&with_remap)
    } else {
        with_remap
    }
}

/// Diagnostic reason a crate gets no default `--bin <crate>` build: a pure
/// library (no binary targets at all) versus a library that carries only
/// helper binaries whose names don't match the crate (so cargo would reject
/// `--bin <crate>`). Surfaced in the skip line so a consumer can tell the two
/// apart at a glance.
fn no_default_binary_reason(crate_path: &str, crate_name: &str) -> String {
    if crate_has_binary_target(crate_path) {
        format!("no binary target named '{crate_name}' (only differently-named helper binaries)")
    } else {
        format!("no binary target named '{crate_name}' (library crate)")
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
        // Determine builds for this crate via the shared planner SSOT: a
        // non-empty `builds:` list as-is, else a synthesized default `--bin
        // <crate>` build when the crate declares a binary named after itself.
        // A library crate (no bins) OR a library carrying only differently-named
        // helper bins (e.g. cfgd-core's renamed src/bin codegen tools) has no
        // default release binary, so `cargo build --bin <crate>` would fail —
        // skip instead.
        let Some(builds) = planned_builds(crate_cfg) else {
            log.skip_line(
                ctx.options.show_skipped,
                &format!(
                    "skipped build for crate '{}' — no builds configured and {}",
                    crate_cfg.name,
                    no_default_binary_reason(&crate_cfg.path, &crate_cfg.name)
                ),
            );
            continue;
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

            // If this build has no explicit `binary:`, the name falls back to
            // the crate's own name — so skip it unless the crate declares a
            // binary target with that name. This protects library crates that
            // inherited a `defaults.builds:` template: whether the crate has no
            // bins at all OR only differently-named helper bins, the fallback
            // `cargo build --bin <crate>` would fail with `no bin target named
            // '<crate>'`. An explicit `binary:` is left to cargo to resolve.
            if build.binary.is_none() && !crate_declares_bin(&crate_cfg.path, &crate_cfg.name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &format!(
                        "skipped build for crate '{}' — no explicit binary and {}",
                        crate_cfg.name,
                        no_default_binary_reason(&crate_cfg.path, &crate_cfg.name)
                    ),
                );
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
                    "skipped build '{}' — skip: true",
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
                        "skipped {}/{} — no targets match partial filter",
                        crate_cfg.name, binary_name_raw
                    ));
                    continue;
                }
            }

            // If no targets configured, skip (caller should ensure defaults)
            if targets.is_empty() {
                log.warn(&format!(
                    "skipped {}/{} — no targets configured",
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
                seed_build_target_vars(
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
                    clear_build_target_vars(ctx.template_vars_mut());

                    // Shared decision with the compile path: declared level
                    // wins (with the mismatch warning), detection reads the
                    // RAW merged map — copy_from env is never rendered —
                    // plus the inherited process env the compiling sibling's
                    // cargo saw.
                    let copy_variant = {
                        let empty_env = HashMap::new();
                        build_amd64_variant(
                            build,
                            target,
                            raw_target_env.as_ref().unwrap_or(&empty_env),
                            ctx.env_source(),
                            log,
                        )
                    };
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
                // (key, value before the cascade): env blocks are per-target
                // config, so the injections are undone after the override
                // merge — a later target's `{{ .Env.KEY }}` seeing an earlier
                // target's value is order-dependent nondeterminism.
                let mut cascade_touched: Vec<(String, Option<String>)> = Vec::new();
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
                    cascade_touched
                        .push((k.clone(), ctx.template_vars().all_env().get(k).cloned()));
                    ctx.template_vars_mut().set_env(k, &expanded);
                    rendered_env.insert(k.clone(), expanded);
                }
                target_env = rendered_env;

                // Merge override env if matched (may reference the cascade,
                // so it renders before the injections are undone).
                let override_merge = (|| -> Result<()> {
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
                    Ok(())
                })();
                for (k, prior) in cascade_touched {
                    match prior {
                        Some(p) => ctx.template_vars_mut().set_env(&k, &p),
                        None => {
                            ctx.template_vars_mut().unset_env(&k);
                        }
                    }
                }
                override_merge?;

                // Set per-target hook context: Name, Path, Ext
                ctx.template_vars_mut().set("Name", &binary_name);
                ctx.template_vars_mut()
                    .set("Path", &bin_path.to_string_lossy());
                ctx.template_vars_mut()
                    .set("Ext", if os == "windows" { ".exe" } else { "" });

                // Remove per-target template variables to avoid leaking
                clear_build_target_vars(ctx.template_vars_mut());
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
                        target,
                    );
                    target_env.insert("RUSTFLAGS".to_string(), new_rustflags);

                    // `/Brepro` and its sibling RUSTFLAGS above guard rustc/
                    // link.exe; they never reach a C compile. cc-rs/cmake
                    // invoke `cl.exe` directly, and `cl.exe`'s object codegen
                    // has a proven register-allocation coin-flip across
                    // otherwise-identical rebuilds (zstd-sys, ring,
                    // aws-lc-sys, ...). Pinning clang-cl here keeps a real
                    // `anodizer release` build byte-identical to what the
                    // determinism harness already proves reproducible, since
                    // a release publishes the shards' preserved dist rather
                    // than rebuilding (anodizer_core::determinism::msvc_c_toolchain_env
                    // no-ops for non-msvc targets).
                    for (key, value) in anodizer_core::determinism::msvc_c_toolchain_env(target) {
                        target_env.insert(key, value);
                    }
                }

                // Surface a doomed routing decision before cargo spends
                // minutes compiling: a cross-arch linux-gnu target on plain
                // cargo fails at the first native-code dependency unless a
                // system cross cc exists, and the resulting cc-rs error
                // doesn't name the missing tooling layer. A `cross_tool`
                // override bypasses strategy resolution entirely.
                if cross_tool.is_none() {
                    let resolved = crate::command::resolved_strategy_for_target(&strategy, target);
                    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
                    if let Some(msg) =
                        crate::command::cross_gnu_cargo_fallback_warning(&host, target, &resolved)
                    {
                        log.warn(&msg);
                    }
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
                    amd64_variant: build_amd64_variant(
                        build,
                        target,
                        &target_env,
                        ctx.env_source(),
                        log,
                    ),
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
            "skipped prebuilt build '{}' — skip: true",
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
                "skipped {}/{} — no prebuilt targets match partial filter",
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

        seed_build_target_vars(
            ctx.template_vars_mut(),
            target,
            &os,
            build.id.as_deref().unwrap_or(""),
        );
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

        clear_build_target_vars(ctx.template_vars_mut());

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

        let amd64_variant = prebuilt_amd64_variant(build, target);

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
    // A non-MSVC target so the MSVC determinism merge stays off — these cases
    // exercise the remap/precedence logic in isolation.
    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const WIN_MSVC: &str = "x86_64-pc-windows-msvc";

    #[test]
    fn preserves_inherited_msvc_flags_from_harness() {
        // The determinism harness injects /Brepro into the child's process
        // RUSTFLAGS. With no per-target config override, the build stage must
        // carry it through — clobbering it reintroduces the PE timestamp drift.
        let inherited = "-C link-arg=/Brepro -C link-arg=/DEBUG:NONE";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, LINUX);
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
            LINUX,
        );
        assert_eq!(merged, format!("-C target-cpu=native {REMAP}"));
    }

    #[test]
    fn remap_only_when_nothing_inherited() {
        assert_eq!(merge_reproducible_rustflags(None, None, CWD, LINUX), REMAP);
        // Blank (whitespace-only) values are treated as unset, not as a real
        // base to append to — no leading-space artifact.
        assert_eq!(
            merge_reproducible_rustflags(Some(""), Some("  "), CWD, LINUX),
            REMAP
        );
    }

    #[test]
    fn does_not_double_remap_when_cwd_already_remapped() {
        // The harness already remaps the worktree (== cwd) to /anodize.
        // A second rule for the same prefix is shadowed (rustc first-match-
        // wins) and only misleads, so it must not be appended.
        let inherited = "-C link-arg=/Brepro --remap-path-prefix=/work=/anodize";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, LINUX);
        assert!(
            merged.starts_with(inherited),
            "must not append a shadowed cwd remap: {merged}"
        );
        assert_eq!(
            merged.matches("--remap-path-prefix=/work=").count(),
            1,
            "exactly one remap rule for the cwd prefix: {merged}"
        );
    }

    /// Regression (PE TimeDateStamp drift): a `reproducible: true` build
    /// targeting `x86_64-pc-windows-msvc` must emit the full MSVC
    /// determinism flag set — keyed on the TARGET triple, so it fires even
    /// when cross-building Windows from a Linux host (where `cfg!(windows)`
    /// is false). Without `/Brepro` the COFF TimeDateStamp at offset 0x108
    /// is wall-clock and the .exe drifts between rebuilds.
    #[test]
    fn windows_msvc_target_gets_full_determinism_flag_set() {
        let merged = merge_reproducible_rustflags(None, None, CWD, WIN_MSVC);
        for needle in [
            "-C codegen-units=1",
            "-C link-arg=/Brepro",
            "-C link-arg=/OPT:NOICF",
            "-C link-arg=/INCREMENTAL:NO",
            "-C link-arg=/DEBUG:NONE",
            "-C strip=symbols",
        ] {
            assert!(
                merged.contains(needle),
                "windows-msvc reproducible build must carry `{needle}`. got={merged}"
            );
        }
        assert!(
            merged.contains(REMAP),
            "remap rule must still be present: {merged}"
        );
    }

    /// A non-MSVC target must NOT receive the MSVC-linker-only flags —
    /// `/Brepro` and the `/...` link args make lld / ld error.
    #[test]
    fn non_msvc_target_gets_no_brepro() {
        let merged = merge_reproducible_rustflags(None, None, CWD, LINUX);
        assert!(
            !merged.contains("/Brepro"),
            "linux target must not carry the MSVC-only /Brepro: {merged}"
        );
        assert_eq!(
            merged, REMAP,
            "linux reproducible build is remap-only: {merged}"
        );
    }

    /// Aarch64 Windows-MSVC is also covered by the target-keyed gate.
    #[test]
    fn aarch64_windows_msvc_target_gets_brepro() {
        let merged = merge_reproducible_rustflags(None, None, CWD, "aarch64-pc-windows-msvc");
        assert!(merged.contains("-C link-arg=/Brepro"), "got={merged}");
    }

    /// Idempotence: an inherited MSVC set (e.g. from the harness env) is not
    /// duplicated when the target-keyed merge runs over it.
    #[test]
    fn windows_msvc_merge_does_not_duplicate_inherited_brepro() {
        let inherited = "-C codegen-units=1 -C link-arg=/Brepro";
        let merged = merge_reproducible_rustflags(None, Some(inherited), CWD, WIN_MSVC);
        assert_eq!(
            merged.matches("/Brepro").count(),
            1,
            "/Brepro must appear exactly once: {merged}"
        );
        assert_eq!(
            merged.matches("codegen-units=1").count(),
            1,
            "codegen-units=1 must appear exactly once: {merged}"
        );
    }
}

#[cfg(test)]
mod per_target_var_tests {
    use anodizer_core::build_env::{
        BUILD_TARGET_VARS, clear_build_target_vars, seed_build_target_vars,
    };
    use anodizer_core::template::TemplateVars;

    fn vars_for(target: &str, os: &str, id: &str) -> TemplateVars {
        let mut v = TemplateVars::new();
        seed_build_target_vars(&mut v, target, os, id);
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
        // `Arch` carries the composite armv7 token, so `Arm` must stay empty —
        // a seeded Arm would double a `{{ .Arch }}v{{ .Arm }}` binary name to
        // `armv7v7` (same class as the mips guard below).
        let armv7 = vars_for("armv7-unknown-linux-gnueabihf", "linux", "");
        assert_eq!(armv7.get("Arch").map(String::as_str), Some("armv7"));
        assert_eq!(armv7.get("Arm").map(String::as_str), Some(""));
        assert_eq!(
            vars_for("i686-unknown-linux-gnu", "linux", "")
                .get("I386")
                .map(String::as_str),
            Some("sse2")
        );
    }

    #[test]
    fn untagged_x86_64_seeds_amd64_baseline_v1() {
        // The value a binary-name template's `{{ .Amd64 }}` renders must match
        // what the installer stages seed for the same untagged binary — the
        // shared "v1" baseline, not an empty string on one side.
        let v = vars_for("x86_64-unknown-linux-gnu", "linux", "");
        assert_eq!(v.get("Amd64").map(String::as_str), Some("v1"));
    }

    #[test]
    fn mips_targets_seed_no_mips_variant() {
        // Arch carries the whole mips token; Mips must stay empty so name
        // templates appending `{% if Mips %}_{{ Mips }}` never double it.
        let v = vars_for("mips64el-unknown-linux-gnuabi64", "linux", "");
        assert_eq!(v.get("Arch").map(String::as_str), Some("mips64el"));
        assert_eq!(v.get("Mips").map(String::as_str), Some(""));
    }

    #[test]
    fn clear_empties_every_set_var() {
        let mut v = vars_for("x86_64-pc-windows-msvc", "windows", "cli");
        clear_build_target_vars(&mut v);
        for k in BUILD_TARGET_VARS {
            assert_eq!(
                v.get(k).map(String::as_str),
                Some(""),
                "{k} must be cleared"
            );
        }
    }
}

#[cfg(test)]
mod env_scope_tests {
    use super::*;
    use anodizer_core::config::CrateConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";

    /// Target A's env block defines `LEVEL`; target B's `RUSTFLAGS`
    /// references `{{ .Env.LEVEL }}` without defining it — the shape that
    /// leaked A's injection into B's render when the cascade was never
    /// undone.
    fn leaky_crate() -> CrateConfig {
        let mut env = HashMap::new();
        env.insert(
            LINUX.to_string(),
            HashMap::from([
                ("LEVEL".to_string(), "x86-64-v3".to_string()),
                (
                    "RUSTFLAGS".to_string(),
                    "-Ctarget-cpu={{ .Env.LEVEL }}".to_string(),
                ),
            ]),
        );
        env.insert(
            WINDOWS.to_string(),
            HashMap::from([(
                "RUSTFLAGS".to_string(),
                "-Ctarget-cpu={{ .Env.LEVEL }}".to_string(),
            )]),
        );
        CrateConfig {
            name: "myapp".to_string(),
            path: "no-such-dir".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("myapp".to_string()),
                targets: Some(vec![LINUX.to_string(), WINDOWS.to_string()]),
                env: Some(env),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    fn plan(krate: &CrateConfig, ctx: &mut Context) -> (Vec<BuildJob>, Vec<BuildJob>) {
        let log = ctx.logger("build");
        let strategy = ctx.config.default_cross_strategy();
        let inputs = PlanInputs {
            crates: std::slice::from_ref(krate),
            default_targets: &[],
            default_strategy: &strategy,
            default_flags: &None,
            default_ignores: &[],
            default_overrides: &[],
            commit_timestamp: "0",
        };
        plan_build_jobs(ctx, &log, &inputs).expect("planning succeeds")
    }

    /// Env blocks are per-target config: a later target's `Env.LEVEL` must
    /// not resolve to an earlier target's cascade injection, and no cascade
    /// key may survive planning. The projection SSOT must agree with the
    /// planner's stamps on both targets.
    #[test]
    fn cascade_env_injections_are_scoped_per_target() {
        let krate = leaky_crate();
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .sealed_env()
            .build();
        let (jobs, copy_jobs) = plan(&krate, &mut ctx);
        assert!(copy_jobs.is_empty());
        assert_eq!(jobs.len(), 2);
        let linux = jobs.iter().find(|j| j.target == LINUX).unwrap();
        let windows = jobs.iter().find(|j| j.target == WINDOWS).unwrap();
        assert_eq!(
            linux.amd64_variant.as_deref(),
            Some("v3"),
            "the same-block cascade must still resolve"
        );
        assert_eq!(
            windows.amd64_variant, None,
            "the second target's Env.LEVEL must not see the first target's injection"
        );
        assert!(
            !ctx.template_vars().all_env().contains_key("LEVEL"),
            "cascade keys must not survive planning"
        );

        // Planner and config-time projection agree on both targets.
        assert_eq!(
            anodizer_core::build_env::config_time_amd64_variant(&krate, LINUX, &[], &mut ctx)
                .unwrap()
                .as_deref(),
            Some("v3")
        );
        assert_eq!(
            anodizer_core::build_env::config_time_amd64_variant(&krate, WINDOWS, &[], &mut ctx)
                .unwrap(),
            None
        );
    }

    /// A declared `amd64_variant:` overrides env detection on the planner
    /// side, and the projection derives the identical stamp.
    #[test]
    fn declared_amd64_variant_overrides_planner_detection() {
        let mut krate = leaky_crate();
        krate.builds.as_mut().unwrap()[0].amd64_variant =
            Some(anodizer_core::config::Amd64Variant::V2);
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .sealed_env()
            .build();
        let (jobs, _) = plan(&krate, &mut ctx);
        let linux = jobs.iter().find(|j| j.target == LINUX).unwrap();
        assert_eq!(
            linux.amd64_variant.as_deref(),
            Some("v2"),
            "declared level must beat the detected v3"
        );
        assert_eq!(
            anodizer_core::build_env::config_time_amd64_variant(&krate, LINUX, &[], &mut ctx)
                .unwrap()
                .as_deref(),
            Some("v2"),
            "projection must agree with the planner's declared stamp"
        );
    }

    /// A valid level set only on `defaults.builds` reaches the planner
    /// stamp: the defaults merge folds it into the crate's builds, and the
    /// planned job carries it.
    #[test]
    fn defaults_axis_amd64_variant_reaches_the_planner_stamp() {
        use anodizer_core::config::{Amd64Variant, Config, Defaults};
        let mut config = Config {
            project_name: "myapp".to_string(),
            defaults: Some(Defaults {
                builds: Some(BuildConfig {
                    amd64_variant: Some(Amd64Variant::V2),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: "no-such-dir".to_string(),
                builds: Some(vec![BuildConfig {
                    binary: Some("myapp".to_string()),
                    targets: Some(vec![LINUX.to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        anodizer_core::defaults_merge::apply_defaults(&mut config);
        let krate = config.crates[0].clone();
        assert_eq!(
            krate.builds.as_ref().unwrap()[0].amd64_variant,
            Some(Amd64Variant::V2),
            "the defaults merge must fold the level into the crate's builds"
        );

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .sealed_env()
            .build();
        let (jobs, _) = plan(&krate, &mut ctx);
        let linux = jobs.iter().find(|j| j.target == LINUX).unwrap();
        assert_eq!(
            linux.amd64_variant.as_deref(),
            Some("v2"),
            "the planner stamp must carry the defaults-declared level"
        );
    }

    /// The CI mislabel vector: a process `RUSTFLAGS="-Dwarnings"` shadows a
    /// level-carrying config `CARGO_TARGET_<T>_RUSTFLAGS` (cargo's sources
    /// are mutually exclusive), so the binary is BASELINE — the planner must
    /// stamp baseline, not the suppressed v3 that would name the asset
    /// `_amd64v3` around an untuned binary. The projection agrees.
    #[test]
    fn process_rustflags_suppress_tuned_target_var_in_planner_stamp() {
        let mut env = HashMap::new();
        env.insert(
            LINUX.to_string(),
            HashMap::from([(
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v3".to_string(),
            )]),
        );
        let krate = CrateConfig {
            name: "myapp".to_string(),
            path: "no-such-dir".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("myapp".to_string()),
                targets: Some(vec![LINUX.to_string()]),
                env: Some(env),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .env("RUSTFLAGS", "-Dwarnings")
            .build();
        let (jobs, _) = plan(&krate, &mut ctx);
        let linux = jobs.iter().find(|j| j.target == LINUX).unwrap();
        assert_eq!(
            linux.amd64_variant, None,
            "the suppressed target var must not stamp v3 on a baseline binary"
        );
        assert_eq!(
            anodizer_core::build_env::config_time_amd64_variant(&krate, LINUX, &[], &mut ctx)
                .unwrap(),
            None,
            "projection must agree with the planner's baseline stamp"
        );
    }

    fn reproducible_crate(target: &str) -> CrateConfig {
        CrateConfig {
            name: "myapp".to_string(),
            path: "no-such-dir".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("myapp".to_string()),
                targets: Some(vec![target.to_string()]),
                reproducible: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// A `reproducible: true` windows-msvc build must carry the clang-cl
    /// pin alongside RUSTFLAGS — see `anodizer_core::determinism::msvc_c_toolchain_env`.
    #[test]
    fn reproducible_windows_msvc_build_pins_clang_cl() {
        let krate = reproducible_crate(WINDOWS);
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .sealed_env()
            .build();
        let (jobs, _) = plan(&krate, &mut ctx);
        let job = jobs.iter().find(|j| j.target == WINDOWS).unwrap();
        let env = &job.cmd.as_ref().unwrap().env;
        for key in [
            "CC_x86_64-pc-windows-msvc",
            "CC_x86_64_pc_windows_msvc",
            "CXX_x86_64-pc-windows-msvc",
            "CXX_x86_64_pc_windows_msvc",
        ] {
            assert_eq!(
                env.get(key).map(String::as_str),
                Some("clang-cl"),
                "missing/wrong pin for {key}: {env:?}"
            );
        }
    }

    /// A non-msvc reproducible build must not gain any `CC_`/`CXX_` keys —
    /// the pin is windows-msvc-only.
    #[test]
    fn reproducible_linux_build_gets_no_msvc_c_toolchain_pins() {
        let krate = reproducible_crate(LINUX);
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .sealed_env()
            .build();
        let (jobs, _) = plan(&krate, &mut ctx);
        let job = jobs.iter().find(|j| j.target == LINUX).unwrap();
        let env = &job.cmd.as_ref().unwrap().env;
        assert!(
            !env.keys()
                .any(|k| k.starts_with("CC_") || k.starts_with("CXX_")),
            "linux target must carry no CC_/CXX_ pins: {env:?}"
        );
    }
}
