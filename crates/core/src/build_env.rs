//! Per-target build-env resolution and amd64 micro-architecture detection.
//!
//! The build stage resolves each target's effective env by glob-matching
//! `build.env` keys, template-rendering the values (with a same-block
//! `{{ .Env.KEY }}` cascade), merging any matching `build.overrides` env, and
//! detecting the x86-64 micro-architecture level from the resulting
//! `RUSTFLAGS` (`-Ctarget-cpu=x86-64-v{2,3,4}` → the artifact's
//! `amd64_variant` metadata that names v2/v3-tuned assets).
//!
//! Config-time consumers must reproduce that exact projection: the
//! binstall/installer asset-name derivation
//! ([`crate::binstall::crate_archive_asset_names`]) renders release asset
//! names before any artifact exists, so the amd64 level baked into a tuned
//! group's archive name has to be re-derivable from config alone — otherwise
//! every derived `pkg_url` / installer arm for that group 404s. Housing the
//! glob/env/override semantics and the variant detector here (the build stage
//! re-exports them) makes the two passes share one implementation instead of
//! drifting copies.

use std::collections::HashMap;

use anyhow::{Context as _, Result};

use crate::config::{BuildConfig, BuildIgnore, BuildOverride, BuilderKind, CrateConfig};
use crate::context::Context;
use crate::env::parse_env_entries;
use crate::env_expand::expand_env;
use crate::log::StageLogger;
use crate::target::map_target;
use crate::template::TemplateVars;

/// The per-target template vars the build planner seeds before rendering a
/// target's binary name, paths, and env values (`Target`/`Os`/`Arch`, the
/// arch-family variant vars, `ArtifactExt`, `ArtifactID`) — and that the
/// config-time env projection must therefore seed too, so a `build.env` value
/// templated on them renders identically in both passes.
pub const BUILD_TARGET_VARS: &[&str] = &[
    "Target",
    "Os",
    "Arch",
    "Arm64",
    "Arm",
    "Amd64",
    "Mips",
    "I386",
    "ArtifactExt",
    "ArtifactID",
];

/// Seed the build stage's per-target template vars ([`BUILD_TARGET_VARS`])
/// before rendering a target's binary name, paths, and env values.
///
/// `os` is the already-mapped OS (`map_target(target).0`) so callers that
/// need it for other decisions don't re-map. The variant vars come from the
/// shared [`crate::archive_name::seed_variant_vars`] policy; binary names
/// render before the amd64 variant is detected from the resolved env, so
/// `Amd64` carries the `"v1"` baseline here.
pub fn seed_build_target_vars(vars: &mut TemplateVars, target: &str, os: &str, build_id: &str) {
    vars.set("Target", target);
    vars.set("Os", os);
    vars.set("Arch", &map_target(target).1);
    crate::archive_name::seed_variant_vars(vars, target, None);
    vars.set("ArtifactExt", if os == "windows" { ".exe" } else { "" });
    vars.set("ArtifactID", build_id);
}

/// Clear the per-target template vars set by [`seed_build_target_vars`] so
/// they don't leak into the next target's rendering.
pub fn clear_build_target_vars(vars: &mut TemplateVars) {
    for k in BUILD_TARGET_VARS {
        vars.set(k, "");
    }
}

/// Check if a target triple matches any entry in the ignore list.
/// Matching is done by comparing the os and arch components of the target
/// triple.
pub fn is_target_ignored(target: &str, ignores: &[BuildIgnore]) -> bool {
    if ignores.is_empty() {
        return false;
    }
    let (os, arch) = map_target(target);
    ignores.iter().any(|ig| ig.os == os && ig.arch == arch)
}

/// Compile a glob pattern with consistent strict-mode-vs-warn handling
/// across the build-env pattern-matching call sites (per-target env keys,
/// build override `targets`, …).
///
/// Returns `Ok(None)` when the pattern fails to compile in normal mode
/// (after logging a warning); `Err` in strict mode; `Ok(Some(pat))` on
/// success. `label` describes the configuration site that produced the
/// pattern, e.g. `"build.env key"` or `"build override target"`.
pub fn try_compile_glob(
    key: &str,
    label: &str,
    log: &StageLogger,
    strict: bool,
) -> Result<Option<glob::Pattern>> {
    match glob::Pattern::new(key) {
        Ok(pat) => Ok(Some(pat)),
        Err(e) => {
            if strict {
                anyhow::bail!(
                    "build: invalid glob pattern in {} '{}': {} (strict mode)",
                    label,
                    key,
                    e
                );
            }
            log.warn(&format!(
                "invalid glob pattern in {} '{}': {}",
                label, key, e
            ));
            Ok(None)
        }
    }
}

/// Resolve the merged env map for a build target by interpreting each
/// `build.env` key as a glob pattern (matching the same `glob::Pattern`
/// semantic used by [`find_matching_override`] and the `targets:` filter on
/// upx / overrides).
///
/// Tradeoff-free UX win over an exact-key lookup: a user who writes
/// `env: { "*-linux-gnu": { CC: musl-gcc } }` gets that env applied to
/// every linux-gnu target instead of silently nothing. Exact target strings
/// are valid trivial globs and match exactly.
///
/// **Merge order is alphabetic, not most-specific-wins.** Keys are visited in
/// lexicographic order; later (alphabetically-greater) matching keys override
/// earlier ones on conflicting values. With both `*-linux-gnu` and the exact
/// target string matching, the exact key sorts later and wins coincidentally.
/// For two glob keys (e.g. `*-linux-gnu` and `x86_64-*`), ASCII order — not
/// pattern specificity — decides. Authors of multiple overlapping keys must
/// keep that in mind; prefer non-overlapping patterns or rely on the exact
/// target string to override globs.
///
/// Returns `Ok(None)` when the env map is absent / empty / has no matching
/// keys; otherwise `Ok(Some(merged))`.
pub fn resolve_target_env(
    env: Option<&HashMap<String, HashMap<String, String>>>,
    target: &str,
    log: &StageLogger,
    strict: bool,
) -> Result<Option<HashMap<String, String>>> {
    let Some(env) = env else { return Ok(None) };
    if env.is_empty() {
        return Ok(None);
    }
    let mut sorted_keys: Vec<&String> = env.keys().collect();
    sorted_keys.sort();
    let mut merged: HashMap<String, String> = HashMap::new();
    let mut matched_any = false;
    for key in sorted_keys {
        let Some(pat) = try_compile_glob(key, "build.env key", log, strict)? else {
            continue;
        };
        if pat.matches(target)
            && let Some(vals) = env.get(key)
        {
            matched_any = true;
            for (k, v) in vals {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(if matched_any { Some(merged) } else { None })
}

/// Find the first matching override for a target triple.
/// Override `targets` are glob patterns matched against the full triple
/// string.
pub fn find_matching_override<'a>(
    target: &str,
    overrides: &'a [BuildOverride],
    log: &StageLogger,
    strict: bool,
) -> Result<Option<&'a BuildOverride>> {
    for ov in overrides {
        for pat_str in &ov.targets {
            let Some(pat) = try_compile_glob(pat_str, "build override target", log, strict)? else {
                continue;
            };
            if pat.matches(target) {
                return Ok(Some(ov));
            }
        }
    }
    Ok(None)
}

/// Extract the x86-64 micro-architecture level from a RUSTFLAGS string:
/// `-Ctarget-cpu=x86-64-v3`, `-C target-cpu=x86-64-v2`, and rustc's long
/// spellings `--codegen target-cpu=…` / `--codegen=target-cpu=…` →
/// `Some("v2"/"v3"/…)`. `None` for any other CPU (`native`, a concrete
/// model, …) or when no `target-cpu` flag is present — only the generic
/// `x86-64-v{N}` levels name release assets.
pub fn amd64_variant_from_rustflags(rustflags: &str) -> Option<String> {
    let tokens: Vec<&str> = rustflags.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let cpu = if let Some(val) = tokens[i].strip_prefix("-Ctarget-cpu=") {
            Some(val)
        } else if let Some(val) = tokens[i]
            .strip_prefix("--codegen=")
            .and_then(|opt| opt.strip_prefix("target-cpu="))
        {
            Some(val)
        } else if (tokens[i] == "-C" || tokens[i] == "--codegen")
            && i + 1 < tokens.len()
            && let Some(val) = tokens[i + 1].strip_prefix("target-cpu=")
        {
            i += 1;
            Some(val)
        } else {
            None
        };
        if let Some(cpu) = cpu
            && let Some(level) = cpu.strip_prefix("x86-64-")
        {
            return Some(level.to_string());
        }
        i += 1;
    }
    None
}

/// The `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` env key for `target`: uppercased
/// with `-`/`.` mapped to `_`. A glibc-suffixed triple (`…-gnu.2.17`) keys by
/// its base triple — the `--target` cargo is actually invoked with.
pub fn cargo_target_rustflags_key(target: &str) -> String {
    let base = target.split('.').next().unwrap_or(target);
    format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        base.to_uppercase().replace('-', "_")
    )
}

/// The extra-rustc-flags string cargo would apply to a build of `target`
/// given `env`, following cargo's documented source order — the sources are
/// MUTUALLY EXCLUSIVE, first present wins: `CARGO_ENCODED_RUSTFLAGS`, then
/// `RUSTFLAGS`, then `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`. A set-but-empty
/// earlier source still shadows the later ones (verified against cargo
/// 1.96.0: `RUSTFLAGS=""` suppresses a level-carrying
/// `CARGO_TARGET_*_RUSTFLAGS` entirely).
fn effective_rustflags(target: &str, env: &HashMap<String, String>) -> Option<String> {
    if let Some(encoded) = env.get("CARGO_ENCODED_RUSTFLAGS") {
        // Encoded form separates argv tokens with 0x1F; target-cpu tokens
        // never contain spaces, so a space join feeds the same tokens to the
        // whitespace-splitting parser.
        return Some(encoded.replace('\u{1f}', " "));
    }
    if let Some(flags) = env.get("RUSTFLAGS") {
        return Some(flags.clone());
    }
    env.get(&cargo_target_rustflags_key(target)).cloned()
}

/// Detect the amd64 micro-architecture variant for a build of `target` from
/// its resolved env map — the value the build stage stamps into the
/// artifact's `amd64_variant` metadata. The flags string is picked by
/// cargo's own source precedence ([`effective_rustflags`]). `None` for
/// non-x86_64 targets and for env whose effective flags carry no level (the
/// untuned baseline).
pub fn amd64_variant_from_env(target: &str, env: &HashMap<String, String>) -> Option<String> {
    if !target.starts_with("x86_64") {
        return None;
    }
    effective_rustflags(target, env).and_then(|flags| amd64_variant_from_rustflags(&flags))
}

/// The user-declared micro-architecture level for a build of `target`
/// (`build.amd64_variant`), arch-gated the same way detection is: the level
/// is an x86-64 dimension, so a declaration on a build whose matrix also
/// covers other arches must not stamp them.
pub fn declared_amd64_variant(build: &BuildConfig, target: &str) -> Option<String> {
    if !target.starts_with("x86_64") {
        return None;
    }
    build.amd64_variant.clone()
}

/// The amd64 variant the build planner stamps on a compiled (or copied)
/// binary of `target`: the declared `build.amd64_variant` when set, else
/// detection from the resolved env map. The single decision shared by the
/// planner and the config-time projection.
pub fn build_amd64_variant(
    build: &BuildConfig,
    target: &str,
    env: &HashMap<String, String>,
) -> Option<String> {
    declared_amd64_variant(build, target).or_else(|| amd64_variant_from_env(target, env))
}

/// The amd64 variant the planner stamps on a `builder: prebuilt` import of
/// `target`: the declared `build.amd64_variant` when set, else the `"v1"`
/// baseline for x86-64 triples — an imported binary's env is never resolved,
/// so nothing can be detected. `None` for other arches.
pub fn prebuilt_amd64_variant(build: &BuildConfig, target: &str) -> Option<String> {
    if target.split('-').next() == Some("x86_64") {
        Some(
            build
                .amd64_variant
                .clone()
                .unwrap_or_else(|| "v1".to_string()),
        )
    } else {
        None
    }
}

/// Derive, from config alone, the amd64 micro-architecture variant the build
/// stage will detect for `crate_cfg`'s build of `target` — the config-time
/// twin of the artifact `amd64_variant` metadata that names a v2/v3-tuned
/// group's release assets.
///
/// Walks the crate's planned builds in the planner's artifact-registration
/// order and projects the first producing, non-skipped, non-ignored build
/// that covers `target` — the same build whose binary the archive stage
/// names the target's group by. `builder: prebuilt` builds mirror the
/// planner's stamp (declared level, else the `"v1"` baseline — no env is
/// ever resolved for an import); compile builds mirror the planner's env
/// resolution: the per-target vars seeded before rendering
/// ([`seed_build_target_vars`]), glob-merged `build.env`
/// ([`resolve_target_env`]), template-rendered values with the same-block
/// `{{ .Env.KEY }}` cascade (falling back to the raw string when a value
/// fails to render, exactly as the planner does), matching `build.overrides`
/// env merged on top, and — for `reproducible: true` builds with no config
/// `RUSTFLAGS` — the inherited process `RUSTFLAGS` the reproducibility merge
/// would adopt as its base. A declared `build.amd64_variant` short-circuits
/// the projection on every path, exactly as it overrides the planner's
/// detection.
///
/// Every seeded per-target var and env-cascade injection is restored before
/// returning, so the caller's template scope is untouched. Returns
/// `Ok(None)` for non-x86_64 targets, for untuned builds, and when the
/// tuning value is only resolvable at build time (an unrenderable template
/// with no literal `target-cpu` token) — in that residual case the derived
/// asset name falls back to the baseline and the snapshot emission
/// cross-check stays the loud backstop.
pub fn config_time_amd64_variant(
    crate_cfg: &CrateConfig,
    target: &str,
    default_targets: &[String],
    ctx: &mut Context,
) -> Result<Option<String>> {
    if !target.starts_with("x86_64") {
        return Ok(None);
    }
    let Some(builds) = crate::build_plan::planned_builds(crate_cfg) else {
        return Ok(None);
    };
    let default_builds = ctx.config.defaults.as_ref().and_then(|d| d.builds.as_ref());
    let default_ignores: Vec<BuildIgnore> = default_builds
        .and_then(|b| b.ignore.clone())
        .unwrap_or_default();
    let default_overrides: Vec<BuildOverride> = default_builds
        .and_then(|b| b.overrides.clone())
        .unwrap_or_default();
    let log = ctx.logger("build");

    // The archive stage names a target's group by its FIRST binary in
    // artifact-registration order, which is not raw config order: prebuilt
    // imports register during planning, compile jobs are registered in
    // planned order after them, and copy_from jobs drain last. Walk the
    // builds in the same three passes so the projected variant belongs to
    // the same build whose metadata names the group.
    let pass_matches = |pass: usize, b: &BuildConfig| -> bool {
        let prebuilt = matches!(b.builder, Some(BuilderKind::Prebuilt));
        match pass {
            0 => prebuilt,
            1 => !prebuilt && b.copy_from.is_none(),
            _ => !prebuilt && b.copy_from.is_some(),
        }
    };
    for pass in 0..3 {
        for build in &builds {
            if !pass_matches(pass, build) {
                continue;
            }
            if !crate::build_plan::build_produces(crate_cfg, build) {
                continue;
            }
            if build_skipped(build, ctx)? {
                continue;
            }
            // Prebuilt builds have no defaults.targets fallback (explicit
            // `targets:` is validator-enforced), matching the planner.
            let targets: &[String] = if pass == 0 {
                build.targets.as_deref().unwrap_or(&[])
            } else {
                build.targets.as_deref().unwrap_or(default_targets)
            };
            if !targets.iter().any(|t| t == target) {
                continue;
            }
            let ignores = build.ignore.as_deref().unwrap_or(&default_ignores);
            if is_target_ignored(target, ignores) {
                continue;
            }
            if pass == 0 {
                return Ok(prebuilt_amd64_variant(build, target));
            }
            return build_env_amd64_variant(build, target, &default_overrides, &log, ctx);
        }
    }
    Ok(None)
}

/// Evaluate a build's `skip:` (bool or template) the way the build planner
/// does — a skipped build compiles nothing, so its env must not decide the
/// group's variant.
fn build_skipped(build: &BuildConfig, ctx: &Context) -> Result<bool> {
    match build.skip.as_ref() {
        Some(s) => s
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| {
                format!(
                    "build: render skip template for build '{}'",
                    build.id.as_deref().unwrap_or("<unnamed>")
                )
            }),
        None => Ok(false),
    }
}

/// Project one build's per-target env exactly as the build planner resolves
/// it and detect the amd64 variant from the result.
fn build_env_amd64_variant(
    build: &BuildConfig,
    target: &str,
    default_overrides: &[BuildOverride],
    log: &StageLogger,
    ctx: &mut Context,
) -> Result<Option<String>> {
    if let Some(declared) = declared_amd64_variant(build, target) {
        return Ok(Some(declared));
    }
    let strict = ctx.is_strict();
    let raw = resolve_target_env(build.env.as_ref(), target, log, strict)?;

    // `copy_from:` jobs never render their env — the planner detects their
    // variant from the RAW merged map, so this projection must too.
    if build.copy_from.is_some() {
        return Ok(raw.as_ref().and_then(|e| amd64_variant_from_env(target, e)));
    }

    // The planner seeds the per-target vars before rendering env values, so
    // a value templated on `Os`/`Target`/… is a supported contract — seed
    // the SAME set for the CURRENT target, saving the caller's values so
    // this config-time projection leaves its template scope untouched
    // (stale values from a caller's own per-target loop must not decide a
    // different target's render).
    let saved_vars: Vec<(&str, Option<String>)> = BUILD_TARGET_VARS
        .iter()
        .map(|k| (*k, ctx.template_vars().get(k).cloned()))
        .collect();
    let (os, _) = map_target(target);
    seed_build_target_vars(
        ctx.template_vars_mut(),
        target,
        &os,
        build.id.as_deref().unwrap_or(""),
    );

    let projected = (|| -> Result<HashMap<String, String>> {
        let mut rendered: HashMap<String, String> = HashMap::new();
        // (key, value before the cascade) so the injections can be undone —
        // the caller's template env must survive this projection too.
        let mut touched: Vec<(String, Option<String>)> = Vec::new();
        if let Some(raw) = &raw {
            let mut keys: Vec<&String> = raw.keys().collect();
            keys.sort();
            for k in keys {
                let v = &raw[k];
                // A value that fails to render falls back to the RAW string
                // (the planner warns and proceeds with it), so detection sees
                // the same bytes in both passes.
                let val = ctx.render_template(v).unwrap_or_else(|_| v.clone());
                let expanded = expand_env(&val);
                touched.push((k.clone(), ctx.template_vars().all_env().get(k).cloned()));
                ctx.template_vars_mut().set_env(k, &expanded);
                rendered.insert(k.clone(), expanded);
            }
        }

        let overrides = build.overrides.as_deref().unwrap_or(default_overrides);
        let override_merge = (|| -> Result<()> {
            if let Some(ov) = find_matching_override(target, overrides, log, strict)?
                && let Some(ov_env) = &ov.env
            {
                for (k, v) in parse_env_entries(ov_env)? {
                    let val = ctx.render_template(&v).unwrap_or_else(|_| v.clone());
                    rendered.insert(k, expand_env(&val));
                }
            }
            Ok(())
        })();

        for (k, prior) in touched {
            match prior {
                Some(p) => ctx.template_vars_mut().set_env(&k, &p),
                None => {
                    ctx.template_vars_mut().unset_env(&k);
                }
            }
        }
        override_merge?;
        Ok(rendered)
    })();

    for (k, prior) in saved_vars {
        match prior {
            Some(p) => ctx.template_vars_mut().set(k, &p),
            None => {
                ctx.template_vars_mut().unset(k);
            }
        }
    }
    let mut rendered = projected?;

    // A reproducible build ALWAYS carries a merged `RUSTFLAGS` into cargo's
    // env (remap rules at minimum), which shadows any
    // `CARGO_TARGET_*_RUSTFLAGS` under cargo's mutually-exclusive source
    // order — mirror that by materializing the merge's base: the config
    // value when non-blank, else the inherited process value. The remap /
    // MSVC flags the merge appends can never carry a `-Ctarget-cpu=` token,
    // so the base alone decides the variant.
    if build.reproducible.unwrap_or(false)
        && rendered
            .get("RUSTFLAGS")
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    {
        rendered.insert(
            "RUSTFLAGS".to_string(),
            ctx.env_source().var("RUSTFLAGS").unwrap_or_default(),
        );
    }
    Ok(amd64_variant_from_env(target, &rendered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Defaults};
    use crate::context::{Context, ContextOptions};
    use crate::env_source::MapEnvSource;

    const LINUX_AMD64: &str = "x86_64-unknown-linux-gnu";

    fn ctx() -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx
    }

    fn crate_with_build(build: BuildConfig) -> CrateConfig {
        CrateConfig {
            name: "myapp".to_string(),
            builds: Some(vec![build]),
            ..Default::default()
        }
    }

    fn tuned_build(target_key: &str, rustflags: &str) -> BuildConfig {
        let mut env = HashMap::new();
        env.insert(
            target_key.to_string(),
            HashMap::from([("RUSTFLAGS".to_string(), rustflags.to_string())]),
        );
        BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            env: Some(env),
            ..Default::default()
        }
    }

    #[test]
    fn detects_variant_from_static_env() {
        let krate = crate_with_build(tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3"));
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );
    }

    #[test]
    fn detects_variant_via_glob_env_key() {
        let krate = crate_with_build(tuned_build("x86_64-*", "-C target-cpu=x86-64-v2"));
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn non_x86_64_targets_never_derive_a_variant() {
        let mut build = tuned_build("*", "-Ctarget-cpu=x86-64-v3");
        build.targets = Some(vec!["aarch64-unknown-linux-gnu".to_string()]);
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, "aarch64-unknown-linux-gnu", &[], &mut c).unwrap(),
            None
        );
    }

    #[test]
    fn untuned_env_derives_no_variant() {
        let build = BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            ..Default::default()
        };
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap(),
            None
        );
    }

    #[test]
    fn templated_env_renders_before_detection() {
        // The tuning level arrives through a template — the projection must
        // render it the way the planner does, not read the raw string.
        let mut c = ctx();
        c.template_vars_mut().set("TuneLevel", "3");
        let krate = crate_with_build(tuned_build(
            LINUX_AMD64,
            "-Ctarget-cpu=x86-64-v{{ TuneLevel }}",
        ));
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );
    }

    #[test]
    fn unrenderable_env_falls_back_to_raw_string_like_the_planner() {
        // Two halves of the planner's raw-fallback contract: an unrenderable
        // value that still literally carries the token detects it; one whose
        // token only exists post-render detects nothing.
        let krate = crate_with_build(tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v2 {{ Broken"));
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2"),
            "raw fallback must preserve a literal target-cpu token"
        );

        let krate = crate_with_build(tuned_build(LINUX_AMD64, "{{ BuildTimeOnlyVar }}"));
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap(),
            None,
            "a build-time-only tuning value derives no variant (loud backstop remains)"
        );
    }

    #[test]
    fn override_env_participates_in_detection() {
        let build = BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            overrides: Some(vec![BuildOverride {
                targets: vec!["x86_64-*".to_string()],
                env: Some(vec!["RUSTFLAGS=-Ctarget-cpu=x86-64-v4".to_string()]),
                flags: None,
                features: None,
            }]),
            ..Default::default()
        };
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v4")
        );
    }

    #[test]
    fn defaults_builds_overrides_apply_when_build_has_none() {
        let build = BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            ..Default::default()
        };
        let krate = crate_with_build(build);
        let mut c = ctx();
        c.config.defaults = Some(Defaults {
            builds: Some(BuildConfig {
                overrides: Some(vec![BuildOverride {
                    targets: vec![LINUX_AMD64.to_string()],
                    env: Some(vec!["RUSTFLAGS=-Ctarget-cpu=x86-64-v3".to_string()]),
                    flags: None,
                    features: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );
    }

    #[test]
    fn reproducible_build_adopts_inherited_rustflags_base() {
        // reproducible: true with no config RUSTFLAGS — the planner's merge
        // adopts the process env value as its base, so a harness/operator
        // `-Ctarget-cpu=x86-64-v3` names the assets and the projection must
        // see it. A non-reproducible build never reads the inherited value
        // (the planner's detection only sees the config env).
        let mut build = BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            reproducible: Some(true),
            ..Default::default()
        };
        let krate = crate_with_build(build.clone());
        let mut c = ctx();
        c.set_env_source(MapEnvSource::new().with("RUSTFLAGS", "-Ctarget-cpu=x86-64-v3"));
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );

        build.reproducible = None;
        let krate = crate_with_build(build);
        let mut c = ctx();
        c.set_env_source(MapEnvSource::new().with("RUSTFLAGS", "-Ctarget-cpu=x86-64-v3"));
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap(),
            None,
            "non-reproducible builds must not read the inherited RUSTFLAGS"
        );
    }

    #[test]
    fn skipped_and_target_ignored_builds_do_not_decide_the_variant() {
        use crate::config::StringOrBool;
        // First build is skip: true (v3), second is live (v2): the live build
        // owns the group.
        let mut skipped = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        skipped.skip = Some(StringOrBool::Bool(true));
        let live = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v2");
        let krate = CrateConfig {
            name: "myapp".to_string(),
            builds: Some(vec![skipped, live]),
            ..Default::default()
        };
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2")
        );

        // A build whose ignore rule excludes the target contributes nothing.
        let mut ignored = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        ignored.ignore = Some(vec![BuildIgnore {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
        }]);
        let krate = crate_with_build(ignored);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap(),
            None
        );
    }

    #[test]
    fn detects_long_form_codegen_flags() {
        // rustc's long spellings of -C, both the two-token and `=` forms.
        assert_eq!(
            amd64_variant_from_rustflags("--codegen target-cpu=x86-64-v3").as_deref(),
            Some("v3")
        );
        assert_eq!(
            amd64_variant_from_rustflags("--codegen=target-cpu=x86-64-v2").as_deref(),
            Some("v2")
        );
        assert_eq!(
            amd64_variant_from_rustflags("--codegen opt-level=3").as_deref(),
            None
        );
    }

    #[test]
    fn detects_cargo_target_rustflags_env_var() {
        let env = HashMap::from([(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
            "-Ctarget-cpu=x86-64-v3".to_string(),
        )]);
        assert_eq!(
            amd64_variant_from_env(LINUX_AMD64, &env).as_deref(),
            Some("v3")
        );
        // The env key belongs to a different triple: not cargo's source for
        // this build.
        assert_eq!(amd64_variant_from_env("x86_64-apple-darwin", &env), None);
        // A glibc-suffixed triple keys by its base triple — the `--target`
        // cargo is invoked with.
        assert_eq!(
            amd64_variant_from_env("x86_64-unknown-linux-gnu.2.17", &env).as_deref(),
            Some("v3")
        );
    }

    #[test]
    fn rustflags_shadows_cargo_target_rustflags() {
        // Cargo's extra-flags sources are mutually exclusive, first present
        // wins (empirically pinned against cargo 1.96.0: with both set, only
        // RUSTFLAGS reaches rustc; RUSTFLAGS="" still suppresses the
        // target-specific var).
        let both = HashMap::from([
            (
                "RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v2".to_string(),
            ),
            (
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v3".to_string(),
            ),
        ]);
        assert_eq!(
            amd64_variant_from_env(LINUX_AMD64, &both).as_deref(),
            Some("v2"),
            "RUSTFLAGS must win when both are present"
        );

        let empty_shadows = HashMap::from([
            ("RUSTFLAGS".to_string(), "".to_string()),
            (
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v3".to_string(),
            ),
        ]);
        assert_eq!(
            amd64_variant_from_env(LINUX_AMD64, &empty_shadows),
            None,
            "a set-but-empty RUSTFLAGS still shadows the target-specific var"
        );

        let encoded_wins = HashMap::from([
            (
                "CARGO_ENCODED_RUSTFLAGS".to_string(),
                "-C\u{1f}target-cpu=x86-64-v4".to_string(),
            ),
            (
                "RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v2".to_string(),
            ),
        ]);
        assert_eq!(
            amd64_variant_from_env(LINUX_AMD64, &encoded_wins).as_deref(),
            Some("v4"),
            "CARGO_ENCODED_RUSTFLAGS outranks RUSTFLAGS"
        );
    }

    #[test]
    fn cargo_target_env_var_flows_through_the_projection() {
        // The SSOT improvement reaches both passes at once: a build tuned
        // ONLY via CARGO_TARGET_<T>_RUSTFLAGS derives its level at config
        // time too.
        let krate = crate_with_build(tuned_build_key(
            LINUX_AMD64,
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "-Ctarget-cpu=x86-64-v2",
        ));
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2")
        );
    }

    fn tuned_build_key(target_key: &str, env_key: &str, value: &str) -> BuildConfig {
        let mut env = HashMap::new();
        env.insert(
            target_key.to_string(),
            HashMap::from([(env_key.to_string(), value.to_string())]),
        );
        BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            env: Some(env),
            ..Default::default()
        }
    }

    #[test]
    fn projection_restores_caller_per_target_vars() {
        // The projection seeds the planner's per-target vars around its env
        // render; the caller's own values (e.g. from ITS per-target loop)
        // must survive untouched, and vars the caller never set must end
        // unset.
        let krate = crate_with_build(tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3"));
        let mut c = ctx();
        c.template_vars_mut().set("Os", "sentinel-os");
        assert!(c.template_vars().get("ArtifactID").is_none());
        let got = config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap();
        assert_eq!(got.as_deref(), Some("v3"));
        assert_eq!(
            c.template_vars().get("Os").map(String::as_str),
            Some("sentinel-os"),
            "caller's Os must be restored"
        );
        assert!(
            c.template_vars().get("ArtifactID").is_none(),
            "vars unset before the projection must end unset"
        );
        assert!(c.template_vars().get("Target").is_none());
    }

    #[test]
    fn prebuilt_builds_mirror_the_planner_stamp() {
        use crate::config::{BuilderKind, PrebuiltConfig};
        // The planner never resolves env for an import: it stamps the "v1"
        // baseline for x86-64 (declared level when set) — a tuning env on
        // the entry must not decide anything.
        let mut build = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        build.builder = Some(BuilderKind::Prebuilt);
        build.prebuilt = Some(PrebuiltConfig {
            path: "out/myapp_{{ Target }}".to_string(),
        });
        let krate = crate_with_build(build.clone());
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v1"),
            "prebuilt x86-64 import carries the planner's v1 stamp, not its env"
        );

        build.amd64_variant = Some("v3".to_string());
        let krate = crate_with_build(build.clone());
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3"),
            "declared level overrides the prebuilt baseline"
        );

        build.amd64_variant = None;
        build.targets = Some(vec!["aarch64-unknown-linux-gnu".to_string()]);
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, "aarch64-unknown-linux-gnu", &[], &mut c).unwrap(),
            None
        );
    }

    #[test]
    fn declared_variant_overrides_detection_and_unrenderable_env() {
        // The escape hatch for tuning env only resolvable at build time:
        // `amd64_variant:` decides without projecting the env at all.
        let mut build = tuned_build(LINUX_AMD64, "{{ BuildTimeOnlyVar }}");
        build.amd64_variant = Some("v3".to_string());
        let krate = crate_with_build(build.clone());
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );

        // Declared beats a CONFLICTING detected level (same precedence as
        // the planner's stamp).
        let mut build = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        build.amd64_variant = Some("v2".to_string());
        let krate = crate_with_build(build.clone());
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2")
        );

        // Arch-gated: the declaration never stamps a non-x86_64 target.
        let mut build = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        build.amd64_variant = Some("v3".to_string());
        build.targets = Some(vec!["aarch64-unknown-linux-gnu".to_string()]);
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, "aarch64-unknown-linux-gnu", &[], &mut c).unwrap(),
            None
        );
    }

    #[test]
    fn projection_follows_artifact_registration_order_not_config_order() {
        use crate::config::{BuilderKind, PrebuiltConfig};
        // The archive group's "first binary" is decided by artifact
        // registration order (prebuilt at plan time, compile jobs next,
        // copy_from jobs last), NOT raw config order. A copy_from entry
        // listed first must not out-vote the compile build that actually
        // registers first.
        let mut copy = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        copy.copy_from = Some("other".to_string());
        let live = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v2");
        let krate = CrateConfig {
            name: "myapp".to_string(),
            builds: Some(vec![copy, live.clone()]),
            ..Default::default()
        };
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v2"),
            "the compile build registers its artifact before the copy_from job"
        );

        // A prebuilt entry listed LAST still registers first (imports are
        // registered during planning, before any job runs).
        let mut prebuilt = BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![LINUX_AMD64.to_string()]),
            ..Default::default()
        };
        prebuilt.builder = Some(BuilderKind::Prebuilt);
        prebuilt.prebuilt = Some(PrebuiltConfig {
            path: "out/myapp".to_string(),
        });
        let krate = CrateConfig {
            name: "myapp".to_string(),
            builds: Some(vec![live, prebuilt]),
            ..Default::default()
        };
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v1"),
            "the prebuilt import's artifact is registered before compile jobs"
        );
    }

    #[test]
    fn copy_from_builds_detect_from_raw_env() {
        // The planner never renders a copy_from job's env: a tuning level
        // that only exists post-render stays undetected for it (the compile
        // path detects it), while a raw literal token still detects. Both
        // halves pin the projection to the planner's raw-map behavior.
        let mut build = tuned_build(LINUX_AMD64, "-Ctarget-cpu={{ CpuVar }}");
        build.copy_from = Some("other".to_string());
        let krate = crate_with_build(build);
        let mut c = ctx();
        c.template_vars_mut().set("CpuVar", "x86-64-v3");
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c).unwrap(),
            None,
            "copy_from must not render env before detection"
        );

        let mut compiled = tuned_build(LINUX_AMD64, "-Ctarget-cpu={{ CpuVar }}");
        compiled.copy_from = None;
        let krate = crate_with_build(compiled);
        let mut c = ctx();
        c.template_vars_mut().set("CpuVar", "x86-64-v3");
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3"),
            "the compile path renders the same value before detection"
        );

        let mut build = tuned_build(LINUX_AMD64, "-Ctarget-cpu=x86-64-v3");
        build.copy_from = Some("other".to_string());
        let krate = crate_with_build(build);
        let mut c = ctx();
        assert_eq!(
            config_time_amd64_variant(&krate, LINUX_AMD64, &[], &mut c)
                .unwrap()
                .as_deref(),
            Some("v3")
        );
    }
}
