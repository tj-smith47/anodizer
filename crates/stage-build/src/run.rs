use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{BuildConfig, BuildIgnore, BuildOverride, CrossStrategy, HookEntry};
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_env as expand_env_vars;
use anodizer_core::hooks::run_hooks;
use anodizer_core::stage::Stage;
use anodizer_core::target::map_target;

use super::command::{
    BuildCommand, build_command, build_lib_command, crate_has_binary_target, detect_crate_type,
};
use super::profile::{detect_amd64_variant, detect_cargo_profile};
use super::targets::{
    DEFAULT_TARGETS, KNOWN_TARGETS, find_matching_override, is_target_ignored, resolve_target_env,
};
use super::universal::{build_universal_binary, project_universal_out_path};
use super::validation::{is_dynamically_linked, strip_glibc_suffix, target_for_validation};
use super::workspace::{
    cargo_target_dir, check_workspace_package, ensure_targets_installed, resolve_binary_path,
    resolve_copy_from, resolve_reproducible_epoch,
};
use super::{binstall, version_sync};

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

        // --- Version sync + binstall: source-mutating steps ---
        // Snapshot builds never mutate source files. The resolved version in
        // snapshot mode is a synthetic identifier (e.g. `0.3.4-SNAPSHOT-abc`),
        // and writing that — or worse, downgrading Cargo.toml when the working
        // tree is ahead of the latest tag — corrupts the working copy. Binstall
        // metadata in snapshot mode would reference a non-existent tag URL.
        let version = ctx
            .template_vars()
            .get("RawVersion")
            .or_else(|| ctx.template_vars().get("Version"))
            .cloned()
            .unwrap_or_default();
        let is_snapshot = ctx.is_snapshot();
        for crate_cfg in &crates {
            if let Some(ref vs) = crate_cfg.version_sync
                && vs.enabled.unwrap_or(false)
            {
                if is_snapshot {
                    log.verbose(&format!(
                        "version-sync: skipping {} (snapshot mode does not mutate source files)",
                        crate_cfg.path
                    ));
                } else if !version.is_empty() {
                    version_sync::sync_version(&crate_cfg.path, &version, dry_run, &log)?;
                }
            }
            if let Some(ref bs) = crate_cfg.binstall
                && bs.enabled.unwrap_or(false)
            {
                if is_snapshot {
                    log.verbose(&format!(
                        "binstall: skipping {} (snapshot mode does not mutate source files)",
                        crate_cfg.path
                    ));
                } else {
                    binstall::generate_binstall_metadata(&crate_cfg.path, bs, ctx, dry_run)?;
                }
            }
        }

        // -----------------------------------------------------------------
        // Flatten the nested (crate, build, target) loops into a list of
        // BuildJob descriptors. No compilation happens here.
        // -----------------------------------------------------------------

        /// A fully-resolved description of one build unit.
        struct BuildJob {
            /// The build command to execute (None for copy_from jobs).
            cmd: Option<BuildCommand>,
            /// For copy_from jobs: source path + destination path.
            copy_from: Option<(PathBuf, PathBuf)>,
            /// Expected output binary path.
            bin_path: PathBuf,
            /// Artifact kind to register.
            artifact_kind: ArtifactKind,
            /// Target triple.
            target: String,
            /// Crate name.
            crate_name: String,
            /// Binary name (for metadata).
            binary_name: String,
            /// Build config ID (for downstream filtering).
            build_id: Option<String>,
            /// Whether reproducible mtime should be applied.
            reproducible: bool,
            /// Pre-build hooks to execute before compilation.
            pre_hooks: Vec<HookEntry>,
            /// Post-build hooks to execute after compilation.
            post_hooks: Vec<HookEntry>,
            /// When true, output binaries to flat dist/ instead of dist/{target}/.
            no_unique_dist_dir: bool,
            /// Crate path (for workspace root resolution).
            crate_path: String,
            /// Optional mod_timestamp override for the built binary.
            mod_timestamp: Option<String>,
            /// Detected amd64 microarchitecture variant (e.g. "v2", "v3", "v4")
            /// from RUSTFLAGS `-C target-cpu=x86-64-vN`.
            amd64_variant: Option<String>,
        }

        /// Result of executing a build job.
        struct BuildResult {
            bin_path: PathBuf,
            artifact_kind: ArtifactKind,
            target: String,
            crate_name: String,
            binary_name: String,
            build_id: Option<String>,
            no_unique_dist_dir: bool,
            amd64_variant: Option<String>,
        }

        /// Builds the metadata map for build-output artifacts. Always populates
        /// the `id` invariant — every artifact created via this helper carries
        /// `id`, defaulting to the binary name when `build.id` is unset.
        ///
        /// Other artifact kinds (Snap, DockerImage, Sbom, …) are constructed
        /// elsewhere and may not carry `id`, so consumers reading
        /// `metadata.get("id")` (e.g. `stage-publish/util.rs`,
        /// `cli/commands/publisher.rs`, `stage-upx`) should still
        /// `Option`-handle unless they know the artifact source kind.
        fn artifact_meta(
            binary: &str,
            build_id: &Option<String>,
            amd64_variant: &Option<String>,
        ) -> HashMap<String, String> {
            // GoReleaser's Build pipe always populates the `id` metadata key
            // (defaults to ProjectName at Default()-time). Mirror that here:
            // if `build.id` is unset, default to the binary name so downstream
            // filters (universal_binaries, archives, signs, …) can rely on
            // `id` being present without falling back to `binary`-key lookups.
            let id = build_id.clone().unwrap_or_else(|| binary.to_string());
            let mut m = HashMap::from([
                ("binary".to_string(), binary.to_string()),
                ("id".to_string(), id),
            ]);
            if let Some(v) = amd64_variant {
                m.insert("amd64_variant".to_string(), v.clone());
            }
            m
        }

        let mut build_jobs: Vec<BuildJob> = Vec::new();
        let mut copy_jobs: Vec<BuildJob> = Vec::new();

        let commit_timestamp = ctx
            .template_vars()
            .get("CommitTimestamp")
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        // Seed the run-wide determinism state from SOURCE_DATE_EPOCH (or the
        // commit timestamp fallback). Downstream stages — stage-sbom in
        // particular — read `ctx.determinism.sde` to derive byte-stable
        // timestamps. Initialized lazily so test contexts without a clean
        // commit can still proceed; missing SDE simply leaves the field as
        // `None` and downstream stages fall back to template vars.
        //
        // Snapshot mode: delegate to `git::resolve_snapshot_sde`, which
        // picks `ANODIZE_SOURCE_DATE_EPOCH` > HEAD timestamp (clean tree)
        // > HEAD + dirty-tree-hash. Replaces the prior "snapshot falls
        // back to commit_timestamp or 0" path so snapshot-mode runs are
        // reproducible across invocations of the same tree state.
        if ctx.determinism.is_none() {
            if ctx.options.snapshot {
                let repo = ctx
                    .options
                    .project_root
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("."));
                match anodizer_core::git::resolve_snapshot_sde(&repo) {
                    Ok(epoch) => {
                        ctx.determinism =
                            Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
                    }
                    Err(err) => {
                        log.status(&format!(
                            "snapshot SDE resolution failed; falling back to commit timestamp: {}",
                            err
                        ));
                        if let Some(epoch) = resolve_reproducible_epoch(&commit_timestamp) {
                            ctx.determinism =
                                Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
                        }
                    }
                }
            } else if let Some(epoch) = resolve_reproducible_epoch(&commit_timestamp) {
                ctx.determinism = Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
            }
        }

        // Append the operator-supplied runtime allow-list (from
        // `--allow-nondeterministic <name>=<reason>` at the CLI) onto
        // the freshly-seeded DeterminismState. Done here (not in the
        // CLI) so the determinism state remains the single source of
        // truth: every downstream consumer (run summary, release body,
        // harness allow-list) reads `ctx.determinism.runtime_allowlist`
        // instead of poking back into `ctx.options`.
        if let Some(state) = ctx.determinism.as_mut() {
            for (name, reason) in &ctx.options.runtime_nondeterministic_allowlist {
                state.append_runtime(name.clone(), reason.clone());
            }
        }

        for crate_cfg in &crates {
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

                // --single-target: filter targets to only the specified triple
                if let Some(ref single) = ctx.options.single_target {
                    let had_targets = !targets.is_empty();
                    targets.retain(|t| t == single);
                    if had_targets && targets.is_empty() {
                        log.warn(&format!(
                            "--single-target: host triple '{}' not in configured targets for {}/{}, skipping",
                            single, crate_cfg.name, binary_name_raw
                        ));
                        continue;
                    }
                }

                // --split: filter targets to those matching the partial target
                if let Some(ref partial) = ctx.options.partial_target {
                    let had_targets = !targets.is_empty();
                    targets = partial.filter_targets(&targets);
                    if had_targets && targets.is_empty() {
                        log.verbose(&format!(
                            "split: no targets match partial filter for {}/{}, skipping",
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

                // Validate targets against known list (error, matching GoReleaser)
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
                    log.warn("both `cross` strategy and `cross_tool` are set; `cross_tool` takes precedence");
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
                        find_matching_override(target, &build_overrides, &log, ctx.is_strict())?;
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
                    let (os, arch) = map_target(target);

                    // Set per-target template vars BEFORE rendering binary name
                    ctx.template_vars_mut().set("Target", target);
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);
                    let first_component = target.split('-').next().unwrap_or("");
                    match first_component {
                        "aarch64" => ctx.template_vars_mut().set("Arm64", "v8"),
                        "armv7" | "armv7l" => ctx.template_vars_mut().set("Arm", "7"),
                        "armv6" | "armv6l" | "arm" => ctx.template_vars_mut().set("Arm", "6"),
                        "x86_64" => ctx.template_vars_mut().set("Amd64", "v1"),
                        "i686" | "i386" | "i586" => ctx.template_vars_mut().set("I386", "sse2"),
                        _ => {}
                    }
                    let artifact_ext = if os == "windows" { ".exe" } else { "" };
                    ctx.template_vars_mut().set("ArtifactExt", artifact_ext);
                    ctx.template_vars_mut()
                        .set("ArtifactID", build.id.as_deref().unwrap_or(""));

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
                        resolve_target_env(build.env.as_ref(), target, &log, ctx.is_strict())?;

                    // Use stripped target name for directory path
                    let bin_path = cargo_target_dir(raw_target_env.as_ref())
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
                        let src_path = cargo_target_dir(raw_target_env.as_ref())
                            .join(cargo_target_name)
                            .join(profile)
                            .join(&src_name);

                        // Clear per-target template vars before continuing
                        ctx.template_vars_mut().set("Target", "");
                        ctx.template_vars_mut().set("Os", "");
                        ctx.template_vars_mut().set("Arch", "");
                        ctx.template_vars_mut().set("Arm64", "");
                        ctx.template_vars_mut().set("Arm", "");
                        ctx.template_vars_mut().set("Amd64", "");
                        ctx.template_vars_mut().set("I386", "");
                        ctx.template_vars_mut().set("ArtifactExt", "");
                        ctx.template_vars_mut().set("ArtifactID", "");

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
                    ctx.template_vars_mut().set("Target", "");
                    ctx.template_vars_mut().set("Os", "");
                    ctx.template_vars_mut().set("Arch", "");
                    ctx.template_vars_mut().set("Arm64", "");
                    ctx.template_vars_mut().set("Arm", "");
                    ctx.template_vars_mut().set("Amd64", "");
                    ctx.template_vars_mut().set("I386", "");
                    ctx.template_vars_mut().set("ArtifactExt", "");
                    ctx.template_vars_mut().set("ArtifactID", "");
                    ctx.template_vars_mut().set("Name", "");
                    ctx.template_vars_mut().set("Path", "");
                    ctx.template_vars_mut().set("Ext", "");

                    // Reproducible builds: inject SOURCE_DATE_EPOCH and RUSTFLAGS
                    if build.reproducible.unwrap_or(false) {
                        target_env
                            .entry("SOURCE_DATE_EPOCH".to_string())
                            .or_insert_with(|| commit_timestamp.clone());

                        let cwd = std::env::current_dir()
                            .unwrap_or_else(|_| PathBuf::from("."))
                            .to_string_lossy()
                            .into_owned();
                        let remap_flag = format!("--remap-path-prefix={cwd}=/build");
                        let existing_rustflags =
                            target_env.get("RUSTFLAGS").cloned().unwrap_or_default();
                        let new_rustflags = if existing_rustflags.is_empty() {
                            remap_flag
                        } else {
                            format!("{existing_rustflags} {remap_flag}")
                        };
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
                    });
                }
            }
        }

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
        // target/ directory locks). GoReleaser explicitly serializes Rust builds
        // for this reason. Force sequential execution unless the user has only
        // a single build job.
        let effective_parallelism = if build_jobs.len() > 1 { 1 } else { parallelism };

        let template_vars = ctx.template_vars().clone();
        let dist_dir = ctx.config.dist.clone();

        // Helper: register a build artifact, respecting no_unique_dist_dir.
        // When no_unique_dist_dir is true, the binary is copied from cargo's
        // target/{triple}/{profile}/ to a flat {dist}/{name} path, and that
        // flattened path is registered as the artifact. In dry-run mode, the
        // flat path is registered without actually copying.
        let add_artifact = |ctx: &mut Context,
                            job_bin_path: &Path,
                            artifact_kind: ArtifactKind,
                            target: &str,
                            crate_name: &str,
                            binary_name: &str,
                            build_id: &Option<String>,
                            no_unique_dist_dir: bool,
                            amd64_variant: &Option<String>|
         -> Result<()> {
            ctx.template_vars_mut().set("Binary", binary_name);
            let mut meta = artifact_meta(binary_name, build_id, amd64_variant);

            let artifact_path = if no_unique_dist_dir {
                meta.insert("no_unique_dist_dir".to_string(), "true".to_string());
                // Flatten: copy binary to dist/{name} instead of keeping the
                // per-target cargo output path.
                let file_name = job_bin_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| binary_name.to_string());
                let flat_path = dist_dir.join(&file_name);
                if !dry_run {
                    if let Some(parent) = flat_path.parent() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create dist dir: {}", parent.display())
                        })?;
                    }
                    if job_bin_path.exists() {
                        std::fs::copy(job_bin_path, &flat_path).with_context(|| {
                            format!(
                                "no_unique_dist_dir: failed to copy {} -> {}",
                                job_bin_path.display(),
                                flat_path.display()
                            )
                        })?;
                    }
                }
                flat_path
            } else {
                job_bin_path.to_path_buf()
            };

            // Check for ELF dynamic linking and store in metadata
            if artifact_path.exists() && is_dynamically_linked(&artifact_path) {
                meta.insert("DynamicallyLinked".to_string(), "true".to_string());
            }

            let artifact_name = artifact_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| artifact_path.display().to_string());
            ctx.artifacts.add(Artifact {
                kind: artifact_kind,
                name: artifact_name,
                path: artifact_path,
                target: Some(target.to_string()),
                crate_name: crate_name.to_string(),
                metadata: meta,
                size: None,
            });
            Ok(())
        };

        if dry_run {
            // Dry-run: just log what would happen, register artifacts sequentially.
            for job in build_jobs.iter().chain(copy_jobs.iter()) {
                // Log pre-hooks (dry-run)
                if !job.pre_hooks.is_empty() {
                    run_hooks(
                        &job.pre_hooks,
                        "pre-build",
                        true,
                        &log,
                        Some(&template_vars),
                    )?;
                }
                if let Some(ref cmd) = job.cmd {
                    log.status(&format!("(dry-run) {} {}", cmd.program, cmd.args.join(" ")));
                } else if let Some((ref src, ref dst)) = job.copy_from {
                    log.status(&format!(
                        "(dry-run) copy {} -> {}",
                        src.display(),
                        dst.display()
                    ));
                }
                // Log post-hooks (dry-run)
                if !job.post_hooks.is_empty() {
                    run_hooks(
                        &job.post_hooks,
                        "post-build",
                        true,
                        &log,
                        Some(&template_vars),
                    )?;
                }
                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        } else if effective_parallelism <= 1 || build_jobs.len() <= 1 {
            // Sequential execution (parallelism == 1 or single job).
            for job in &build_jobs {
                // MkdirAll the dist/target dir BEFORE running pre-hooks, so
                // a pre-hook writing into the expected bin output directory
                // succeeds (GoReleaser build/build.go:147-155 order).
                if let Some(parent) = job.bin_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "failed to create bin output dir: {} (for pre-hook)",
                            parent.display()
                        )
                    })?;
                }
                // Execute pre-build hooks
                if !job.pre_hooks.is_empty() {
                    run_hooks(
                        &job.pre_hooks,
                        "pre-build",
                        false,
                        &log,
                        Some(&template_vars),
                    )?;
                }

                let cmd = job
                    .cmd
                    .as_ref()
                    .context("build job has no cmd (programmer bug: Step 1 should populate)")?;
                log.status(&format!("running: {} {}", cmd.program, cmd.args.join(" ")));
                let output = Command::new(&cmd.program)
                    .args(&cmd.args)
                    .envs(&cmd.env)
                    .current_dir(&cmd.cwd)
                    .output()
                    .with_context(|| format!("failed to spawn {}", cmd.program))?;
                log.check_output(output, &cmd.program)?;

                // Resolve the binary path — try workspace root if not at
                // the expected relative location.
                let resolved_bin = resolve_binary_path(&job.bin_path, &job.crate_path);

                // Verify the binary was actually produced
                if !resolved_bin.exists() {
                    anyhow::bail!(
                        "build succeeded but binary not found at {} (also checked workspace root): \
                         check that the binary name matches your Cargo.toml [bin] section",
                        job.bin_path.display()
                    );
                }

                // Reproducible mtime: set binary mtime to SOURCE_DATE_EPOCH
                if job.reproducible && resolved_bin.exists() {
                    if let Some(epoch) = resolve_reproducible_epoch(&commit_timestamp) {
                        anodizer_core::util::set_file_mtime_epoch(&resolved_bin, epoch)?;
                    } else {
                        log.warn(
                            "reproducible build requested but could not determine epoch \
                             from SOURCE_DATE_EPOCH or CommitTimestamp; mtime will not be set",
                        );
                    }
                }

                // Apply mod_timestamp if configured (overrides reproducible mtime)
                if let Some(ref ts) = job.mod_timestamp
                    && resolved_bin.exists()
                {
                    let rendered_ts = ctx
                        .render_template(ts)
                        .with_context(|| format!("build: render mod_timestamp template '{ts}'"))?;
                    let mtime = anodizer_core::util::parse_mod_timestamp(&rendered_ts)?;
                    anodizer_core::util::set_file_mtime(&resolved_bin, mtime)?;
                    log.verbose(&format!(
                        "applied mod_timestamp={rendered_ts} to {}",
                        resolved_bin.display()
                    ));
                }

                // Execute post-build hooks
                if !job.post_hooks.is_empty() {
                    run_hooks(
                        &job.post_hooks,
                        "post-build",
                        false,
                        &log,
                        Some(&template_vars),
                    )?;
                }

                add_artifact(
                    ctx,
                    &resolved_bin,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }

            // Copy-from jobs (must run after source builds complete)
            for job in &copy_jobs {
                let (src, dst) = job
                    .copy_from
                    .as_ref()
                    .context("copy_from job without copy_from pair (programmer bug)")?;
                resolve_copy_from(ctx, src, dst, &job.target, &job.crate_name)?;

                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        } else {
            // Parallel execution: process build jobs in chunks.
            // Note: pre/post hooks run sequentially before/after each parallel chunk.
            log.status(&format!(
                "building {} jobs with parallelism={}",
                build_jobs.len(),
                effective_parallelism
            ));

            for chunk in build_jobs.chunks(effective_parallelism) {
                // Each chunk runs in parallel via thread::scope.
                // Pre/post hooks run inside each thread so they properly bracket
                // their specific build, matching the sequential path's semantics.
                let results: Vec<Result<BuildResult>> = std::thread::scope(|s| {
                    let handles: Vec<_> = chunk
                        .iter()
                        .map(|job| {
                            // `job.cmd` is populated for every build job (copy-from-only
                            // jobs take a separate code path). If it's absent here, that's a
                            // pipeline invariant violation — surface as an error, not a panic,
                            // so the worker thread unwinds through the Result channel instead
                            // of killing the process.
                            let cmd_opt = job.cmd.clone();
                            let crate_name_for_err = job.crate_name.clone();
                            let program = cmd_opt.as_ref().map(|c| c.program.clone());
                            let args = cmd_opt.as_ref().map(|c| c.args.clone());
                            let env = cmd_opt.as_ref().map(|c| c.env.clone());
                            let cwd = cmd_opt.as_ref().map(|c| c.cwd.clone());
                            let bin_path = job.bin_path.clone();
                            let artifact_kind = job.artifact_kind;
                            let target = job.target.clone();
                            let crate_name = job.crate_name.clone();
                            let binary_name = job.binary_name.clone();
                            let build_id = job.build_id.clone();
                            let reproducible = job.reproducible;
                            let no_unique_dist_dir = job.no_unique_dist_dir;
                            let job_crate_path = job.crate_path.clone();
                            let commit_ts = commit_timestamp.clone();
                            let pre_hooks = job.pre_hooks.clone();
                            let post_hooks = job.post_hooks.clone();
                            let job_mod_timestamp = job.mod_timestamp.clone();
                            let job_amd64_variant = job.amd64_variant.clone();
                            let thread_tvars = template_vars.clone();
                            // Per-thread logger: clone the parent so it inherits
                            // the env-pairs attached for redaction.
                            let thread_log = log.clone();
                            let warn_log = log.clone();

                            s.spawn(move || -> Result<BuildResult> {
                                let program = program.ok_or_else(|| anyhow::anyhow!(
                                    "build: Step 1 invariant violation — job for crate {} reached Step 2 without a cmd",
                                    crate_name_for_err
                                ))?;
                                let args = args.unwrap_or_default();
                                let env = env.unwrap_or_default();
                                let cwd = cwd.unwrap_or_default();

                                // MkdirAll the dist/target dir BEFORE the
                                // pre-hook (GoReleaser build/build.go:147-155
                                // order) so pre-hooks can stage files into the
                                // expected output directory.
                                if let Some(parent) = bin_path.parent() {
                                    std::fs::create_dir_all(parent).with_context(|| {
                                        format!(
                                            "failed to create bin output dir: {} (for pre-hook)",
                                            parent.display()
                                        )
                                    })?;
                                }
                                // Execute pre-build hooks before compilation
                                if !pre_hooks.is_empty() {
                                    run_hooks(&pre_hooks, "pre-build", false, &thread_log, Some(&thread_tvars))?;
                                }

                                thread_log.status(&format!("running: {} {}", program, args.join(" ")));
                                let output = Command::new(&program)
                                    .args(&args)
                                    .envs(&env)
                                    .current_dir(&cwd)
                                    .output()
                                    .with_context(|| format!("failed to spawn {}", program))?;

                                if !output.status.success() {
                                    // Redact secrets in stderr/stdout before
                                    // interpolating into the bail message.
                                    // thread_log inherits the env attached
                                    // at `ctx.logger("build")`.
                                    let stderr = thread_log
                                        .redact(&String::from_utf8_lossy(&output.stderr));
                                    let stdout = thread_log
                                        .redact(&String::from_utf8_lossy(&output.stdout));
                                    let mut msg = format!(
                                        "{} failed with exit code: {}",
                                        program,
                                        output.status.code().unwrap_or(-1)
                                    );
                                    if !stderr.is_empty() {
                                        msg.push_str(&format!("\nstderr:\n{}", stderr));
                                    }
                                    if !stdout.is_empty() {
                                        msg.push_str(&format!("\nstdout:\n{}", stdout));
                                    }
                                    anyhow::bail!("{}", msg);
                                }

                                // Resolve the binary path — try workspace root
                                // if not at the expected relative location.
                                let bin_path = resolve_binary_path(&bin_path, &job_crate_path);

                                // Verify the binary was actually produced
                                if !bin_path.exists() {
                                    anyhow::bail!(
                                        "build succeeded but binary not found at {} (also checked workspace root): \
                                         check that the binary name matches your Cargo.toml [bin] section",
                                        bin_path.display()
                                    );
                                }

                                // Reproducible mtime: set binary mtime to SOURCE_DATE_EPOCH
                                if reproducible && bin_path.exists() {
                                    if let Some(epoch) = resolve_reproducible_epoch(&commit_ts) {
                                        anodizer_core::util::set_file_mtime_epoch(&bin_path, epoch)?;
                                    } else {
                                        warn_log.warn(
                                            "reproducible build requested but could not determine epoch \
                                             from SOURCE_DATE_EPOCH or CommitTimestamp; mtime will not be set",
                                        );
                                    }
                                }

                                // Apply mod_timestamp if configured (overrides reproducible mtime)
                                if let Some(ref ts) = job_mod_timestamp
                                    && bin_path.exists()
                                {
                                    // Thread context doesn't have ctx for template rendering,
                                    // so render using Tera directly with thread-local vars.
                                    let rendered_ts = anodizer_core::template::render(ts, &thread_tvars)
                                        .with_context(|| format!("build: render mod_timestamp template '{ts}'"))?;
                                    let mtime = anodizer_core::util::parse_mod_timestamp(&rendered_ts)?;
                                    anodizer_core::util::set_file_mtime(&bin_path, mtime)?;
                                    thread_log.verbose(&format!(
                                        "applied mod_timestamp={rendered_ts} to {}",
                                        bin_path.display()
                                    ));
                                }

                                // Execute post-build hooks after compilation
                                if !post_hooks.is_empty() {
                                    run_hooks(&post_hooks, "post-build", false, &thread_log, Some(&thread_tvars))?;
                                }

                                Ok(BuildResult {
                                    bin_path,
                                    artifact_kind,
                                    target,
                                    crate_name,
                                    binary_name,
                                    build_id,
                                    no_unique_dist_dir,
                                    amd64_variant: job_amd64_variant,
                                })
                            })
                        })
                        .collect();

                    handles
                        .into_iter()
                        .map(|h| {
                            // Lift a panic into an anyhow::Error tagged with
                            // the stage so the bubbled error names what
                            // crashed, then flatten with the worker's own
                            // Result<T, anyhow::Error>.
                            anodizer_core::parallel::join_panic_to_err(h.join(), "build")
                                .and_then(|r| r)
                        })
                        .collect()
                });

                // Register artifacts sequentially after the chunk completes.
                for result in results {
                    let r = result?;
                    log.status(&format!(
                        "built {}/{} for {}",
                        r.crate_name, r.binary_name, r.target
                    ));
                    add_artifact(
                        ctx,
                        &r.bin_path,
                        r.artifact_kind,
                        &r.target,
                        &r.crate_name,
                        &r.binary_name,
                        &r.build_id,
                        r.no_unique_dist_dir,
                        &r.amd64_variant,
                    )?;
                }
            }

            // Copy-from jobs (must run after source builds complete)
            for job in &copy_jobs {
                let (src, dst) = job
                    .copy_from
                    .as_ref()
                    .context("copy_from job without copy_from pair (programmer bug)")?;
                resolve_copy_from(ctx, src, dst, &job.target, &job.crate_name)?;

                add_artifact(
                    ctx,
                    &job.bin_path,
                    job.artifact_kind,
                    &job.target,
                    &job.crate_name,
                    &job.binary_name,
                    &job.build_id,
                    job.no_unique_dist_dir,
                    &job.amd64_variant,
                )?;
            }
        }

        // --- Universal binaries (macOS lipo) ---
        let mut seen_universal_outputs: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        for crate_cfg in &crates {
            if let Some(ref ub_configs) = crate_cfg.universal_binaries {
                for ub in ub_configs {
                    let projected = project_universal_out_path(crate_cfg.name.as_str(), ub, ctx)?;
                    if let Some(existing) = projected.as_ref()
                        && !seen_universal_outputs.insert(existing.clone())
                    {
                        anyhow::bail!(
                            "build: two universal_binaries entries resolve to the same output path \
                             {:?}; set distinct `name_template` or `id` values to disambiguate",
                            existing
                        );
                    }
                    build_universal_binary(crate_cfg.name.as_str(), ub, ctx, dry_run)?;
                }
            }
        }

        Ok(())
    }
}
