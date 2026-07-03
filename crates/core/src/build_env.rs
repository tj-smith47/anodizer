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

use crate::config::{BuildConfig, BuildIgnore, BuildOverride, CrateConfig};
use crate::context::Context;
use crate::env::parse_env_entries;
use crate::env_expand::expand_env;
use crate::log::StageLogger;
use crate::target::map_target;

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
/// `-Ctarget-cpu=x86-64-v3` / `-C target-cpu=x86-64-v2` → `Some("v2"/"v3")`.
/// `None` for any other CPU (`native`, a concrete model, …) or when no
/// `target-cpu` flag is present — only the generic `x86-64-v{N}` levels name
/// release assets.
pub fn amd64_variant_from_rustflags(rustflags: &str) -> Option<String> {
    let tokens: Vec<&str> = rustflags.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let cpu = if let Some(val) = tokens[i].strip_prefix("-Ctarget-cpu=") {
            Some(val)
        } else if tokens[i] == "-C"
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

/// Detect the amd64 micro-architecture variant for a build of `target` from
/// its resolved env map — the value the build stage stamps into the
/// artifact's `amd64_variant` metadata. `None` for non-x86_64 targets and for
/// env with no level-carrying `RUSTFLAGS` (the untuned baseline).
pub fn amd64_variant_from_env(target: &str, env: &HashMap<String, String>) -> Option<String> {
    if !target.starts_with("x86_64") {
        return None;
    }
    if let Some(flags) = env.get("RUSTFLAGS")
        && let Some(v) = amd64_variant_from_rustflags(flags)
    {
        return Some(v);
    }
    None
}

/// Derive, from config alone, the amd64 micro-architecture variant the build
/// stage will detect for `crate_cfg`'s build of `target` — the config-time
/// twin of the artifact `amd64_variant` metadata that names a v2/v3-tuned
/// group's release assets.
///
/// Walks the crate's planned builds in config order and projects the first
/// producing, non-skipped, non-ignored build that covers `target` — the same
/// build whose binary the archive stage names the target's group by. The
/// projection mirrors the build planner: glob-merged `build.env`
/// ([`resolve_target_env`]), template-rendered values with the same-block
/// `{{ .Env.KEY }}` cascade (falling back to the raw string when a value
/// fails to render, exactly as the planner does), matching `build.overrides`
/// env merged on top, and — for `reproducible: true` builds with no config
/// `RUSTFLAGS` — the inherited process `RUSTFLAGS` the reproducibility merge
/// would adopt as its base.
///
/// Every env-cascade injection is restored before returning, so the caller's
/// template env is untouched. Returns `Ok(None)` for non-x86_64 targets, for
/// untuned builds, and when the tuning value is only resolvable at build time
/// (an unrenderable template with no literal `target-cpu` token) — in that
/// residual case the derived asset name falls back to the baseline and the
/// snapshot emission cross-check stays the loud backstop.
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

    for build in &builds {
        if !crate::build_plan::build_produces(crate_cfg, build) {
            continue;
        }
        if build_skipped(build, ctx)? {
            continue;
        }
        let targets: &[String] = build.targets.as_deref().unwrap_or(default_targets);
        if !targets.iter().any(|t| t == target) {
            continue;
        }
        let ignores = build.ignore.as_deref().unwrap_or(&default_ignores);
        if is_target_ignored(target, ignores) {
            continue;
        }
        // First producing build for the target = the group's first binary —
        // the binary whose metadata the archive stage names the group by.
        return build_env_amd64_variant(build, target, &default_overrides, &log, ctx);
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
    let strict = ctx.is_strict();
    let raw = resolve_target_env(build.env.as_ref(), target, log, strict)?;

    // `copy_from:` jobs never render their env — the planner detects their
    // variant from the RAW merged map, so this projection must too.
    if build.copy_from.is_some() {
        return Ok(raw.as_ref().and_then(|e| amd64_variant_from_env(target, e)));
    }

    let mut rendered: HashMap<String, String> = HashMap::new();
    // (key, value before the cascade) so the injections can be undone — this
    // is a config-time projection; the caller's template env must survive it.
    let mut touched: Vec<(String, Option<String>)> = Vec::new();
    if let Some(raw) = &raw {
        let mut keys: Vec<&String> = raw.keys().collect();
        keys.sort();
        for k in keys {
            let v = &raw[k];
            // A value that fails to render falls back to the RAW string (the
            // planner warns and proceeds with it), so detection sees the same
            // bytes in both passes.
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

    // A reproducible build with no config RUSTFLAGS adopts the inherited
    // process value as its base (the reproducibility merge's precedence); the
    // remap / MSVC flags that merge appends can never carry a `-Ctarget-cpu=`
    // token, so the base alone decides the variant.
    let mut flags = rendered.get("RUSTFLAGS").cloned();
    if build.reproducible.unwrap_or(false)
        && flags.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        flags = ctx.env_source().var("RUSTFLAGS");
    }
    Ok(flags.and_then(|f| amd64_variant_from_rustflags(&f)))
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
