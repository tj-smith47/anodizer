//! Build-synthesis single source of truth: which build entries a crate
//! actually compiles, and over which target triples.
//!
//! Every target and toolchain enumeration MUST resolve through these helpers
//! rather than re-deriving the synthesis rule, so independent call sites cannot
//! drift on which crates build and what they produce. The build planner's
//! per-build compile gate is the reference behavior; [`build_produces`] mirrors
//! it and [`crate_target_list`] composes it with [`planned_builds`].

use std::path::Path;

use crate::config::{BuildConfig, BuilderKind, CrateConfig};
use crate::context::Context;

/// True when the crate at `crate_path` exposes a binary *target* named
/// `wanted` — i.e. `cargo build --bin <wanted>` would resolve. Mirrors
/// `crate_has_binary_target`'s filesystem-probe approach (no `cargo
/// metadata` spawn): an explicit `[[bin]] name = "<wanted>"`, the
/// package-named binary produced by `src/main.rs`, or an auto-discovered
/// `src/bin/<wanted>.rs`.
///
/// Distinct from `crate_has_binary_target`, which answers "does this crate
/// have ANY binary target". A library crate can carry helper binaries whose
/// names do not match the crate (e.g. `src/bin/gen.rs` renamed via `[[bin]]`
/// to `mylib-gen`); such a crate "has a binary target" yet has none named
/// after itself, so a synthesized default `--bin <crate>` build must be
/// suppressed rather than handed to cargo, which would hard-error with
/// `no bin target named '<crate>'` and fail the build/determinism legs.
///
/// Shares `crate_has_binary_target`'s documented `autobins = false`
/// limitation for the `src/bin/` probe. One further filesystem-probe blind
/// spot: a *nameless* `[[bin]]` with a custom `path` outside `src/bin/` (cargo
/// derives that target's name from the path stem) is not detected — covering
/// it would require a `cargo metadata` spawn. Such layouts are rare; declare a
/// `name` to be seen here.
pub fn crate_declares_bin(crate_path: &str, wanted: &str) -> bool {
    let path = Path::new(crate_path);
    let doc = std::fs::read_to_string(path.join("Cargo.toml"))
        .ok()
        .and_then(|c| c.parse::<toml_edit::DocumentMut>().ok());
    let bin_tables = doc
        .as_ref()
        .and_then(|d| d.get("bin"))
        .and_then(|b| b.as_array_of_tables());

    // 1. Explicit `[[bin]] name = "<wanted>"`.
    if let Some(arr) = bin_tables
        && arr
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(wanted))
    {
        return true;
    }

    // 2. `src/main.rs` yields a binary named after the package; it matches
    //    when the package name is `wanted` (the default binary name a
    //    synthesized build resolves to is the crate's own name).
    if path.join("src/main.rs").exists()
        && doc
            .as_ref()
            .and_then(|d| d.get("package"))
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            == Some(wanted)
    {
        return true;
    }

    // 3. Auto-discovered `src/bin/<wanted>.rs` (cargo names the target after
    //    the file stem) — unless an explicit `[[bin]]` re-paths that file to a
    //    *different* name, which removes the stem-named target cargo would have
    //    auto-discovered. Without this guard a crate named after one of its own
    //    renamed helper files would falsely claim the target and re-trigger the
    //    doomed `--bin <wanted>`.
    let stem_file = format!("{wanted}.rs");
    if path.join("src/bin").join(&stem_file).exists() {
        let reclaimed_under_other_name = bin_tables.is_some_and(|arr| {
            arr.iter().any(|t| {
                t.get("name").and_then(|v| v.as_str()) != Some(wanted)
                    && t.get("path")
                        .and_then(|v| v.as_str())
                        .and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_owned))
                        .as_deref()
                        == Some(stem_file.as_str())
            })
        });
        return !reclaimed_under_other_name;
    }
    false
}

/// The build entries the build planner will actually compile for a crate, or
/// `None` when the crate compiles nothing.
///
/// The single source of truth for the "what does this crate produce"
/// synthesis rule:
///
/// - a non-empty `builds:` list is used as-is;
/// - a crate with no `builds:` that declares a `--bin <crate>` target named
///   after itself gets a single synthesized default build whose binary is
///   whatever [`binary_or_crate_name`] resolves for a defaulted entry, with
///   targets inherited from `defaults.targets`;
/// - a crate with neither — a library, or one carrying only differently-named
///   helper bins — compiles nothing and yields `None`.
///
/// Target resolution (per-build `targets` overriding `defaults.targets`) is the
/// caller's concern; this answers only which build entries exist.
pub fn planned_builds(krate: &CrateConfig) -> Option<Vec<BuildConfig>> {
    match krate.builds.as_deref() {
        Some(b) if !b.is_empty() => Some(b.to_vec()),
        _ => crate_declares_bin(&krate.path, &krate.name).then(|| {
            vec![BuildConfig {
                binary: Some(binary_or_crate_name(krate, &BuildConfig::default())),
                ..Default::default()
            }]
        }),
    }
}

/// Whether a build entry yields a shippable artifact (compiled binary or a
/// staged prebuilt). A `defaults.builds:` template materialized onto a library
/// crate carries `binary: None` and resolves no default `--bin <crate>`, so it
/// compiles nothing — the build planner skips it, and every target/toolchain
/// enumeration must skip it identically or it over-reports.
pub fn build_produces(krate: &CrateConfig, build: &BuildConfig) -> bool {
    matches!(build.builder, Some(BuilderKind::Prebuilt))
        || build.binary.is_some()
        || crate_declares_bin(&krate.path, &krate.name)
}

/// A build entry's static id, tagged with whether the caller must render it
/// before comparing against a configured id list.
///
/// Mirrors `stage-build::run_helpers::artifact_meta`'s exact precedence: an
/// explicit `build.id` is stamped onto the `Binary` artifact's `id` metadata
/// byte-for-byte (`run.rs` clones it raw, never through
/// [`crate::context::Context::render_template`]); only the `binary`-fallback
/// id (`build.binary`, or the crate name when `binary` is unset too) is ever
/// rendered, once per target, before it becomes the artifact's `id`. A
/// caller that renders an `Explicit` id anyway would match configured id
/// lists production itself never matches — this module has no `Context` to
/// render through, so the two cases are kept distinguishable rather than
/// collapsed into one already-resolved string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildId {
    /// `build.id` was set; compare this string verbatim, never rendered.
    Explicit(String),
    /// `build.id` was unset; this is the unrendered `binary`-fallback source
    /// (`build.binary` or the crate name). Callers with a live `Context`
    /// must render it the same way `stage-build::run.rs` renders
    /// `binary_name` before comparing or displaying it.
    BinaryFallback(String),
}

impl BuildId {
    /// The raw string this variant carries, unrendered. Correct for
    /// `Explicit` (which is never templated in production); a caller
    /// needing the true resolved value of a `BinaryFallback` must render it
    /// through a [`crate::context::Context`] first.
    pub fn raw(&self) -> &str {
        match self {
            BuildId::Explicit(s) | BuildId::BinaryFallback(s) => s,
        }
    }
}

/// One planned build entry's static identity + the target triples it
/// contributes, as resolved by [`crate_build_target_entries`].
pub struct CrateBuildTargets {
    pub id: BuildId,
    /// The binary the entry compiles: its `binary:`, else the crate's own
    /// `[[bin]]` name. Unrendered — a `binary:` that is itself a template must
    /// be rendered before it is compared or displayed, the same way
    /// [`BuildId::BinaryFallback`] must.
    pub binary: String,
    pub targets: Vec<String>,
}

/// [`crate_target_list`], but callers can additionally veto a build entry
/// (e.g. a truthy `BuildConfig.skip`) and get each surviving build's static
/// id alongside its target triples, not just the flattened union. THE single
/// source of truth for crate target enumeration — [`crate_target_list`] and
/// `stage-publish::publisher_helpers::crate_build_targets` both compose this
/// rather than re-deriving the synthesis rule, so they cannot drift.
pub fn crate_build_target_entries(
    krate: &CrateConfig,
    default_targets: &[String],
    mut is_skipped: impl FnMut(&BuildConfig) -> bool,
) -> Vec<CrateBuildTargets> {
    let Some(builds) = planned_builds(krate) else {
        return Vec::new();
    };
    let mut out: Vec<CrateBuildTargets> = Vec::new();
    for build in &builds {
        if !build_produces(krate, build) || is_skipped(build) {
            continue;
        }
        let chosen: &[String] = match build.targets.as_deref() {
            Some(ts) => ts,
            None => default_targets,
        };
        out.push(CrateBuildTargets {
            id: static_build_id(krate, build),
            binary: binary_or_crate_name(krate, build),
            targets: chosen.to_vec(),
        });
    }
    out
}

/// A build entry's static id, the value `stage-build` stamps onto the
/// artifacts it produces: an explicit `build.id`, else the `binary`-fallback
/// (see [`BuildId`]).
fn static_build_id(krate: &CrateConfig, build: &BuildConfig) -> BuildId {
    match build.id.clone() {
        Some(id) => BuildId::Explicit(id),
        None => BuildId::BinaryFallback(binary_or_crate_name(krate, build)),
    }
}

/// The binary a build entry compiles: its `binary:`, else the crate's own
/// `[[bin]]` target, which is what an entry omitting `binary:` builds. THE
/// spelling of that fallback — a call site re-deriving it drifts the moment
/// the rule does.
pub fn binary_or_crate_name(krate: &CrateConfig, build: &BuildConfig) -> String {
    build.binary.clone().unwrap_or_else(|| krate.name.clone())
}

/// The binary a crate's release is named after when no archive selector
/// narrows the candidates: the first build entry the run actually RELEASES —
/// one that produces an artifact and that `is_skipped` does not veto — else
/// the crate's own `[[bin]]` name. The last resort of every "what is this
/// crate's binary called" chain — the snap name, the installed-version probe.
///
/// Taking the first CONFIGURED build instead names the release after an entry
/// the run never compiles: a `defaults.builds:` template materialized onto a
/// crate carries `binary: None` and resolves no default `--bin <crate>`, so it
/// produces nothing while still sitting first in the list; a `skip:` build
/// compiles nothing for the same reason. Pass [`build_is_skipped`] against a
/// live context for `is_skipped`.
pub fn crate_primary_binary_name(
    krate: &CrateConfig,
    mut is_skipped: impl FnMut(&BuildConfig) -> bool,
) -> String {
    planned_builds(krate)
        .and_then(|builds| {
            builds
                .iter()
                .find(|b| build_produces(krate, b) && !is_skipped(b))
                .map(|b| binary_or_crate_name(krate, b))
        })
        .unwrap_or_else(|| binary_or_crate_name(krate, &BuildConfig::default()))
}

/// Whether an archive's `ids:` filter selects a build entry's id. The
/// static-id half of the artifact-level `matches_id_filter`, which judges the
/// produced artifacts by that same id; an absent or empty list selects every
/// build.
fn archive_selects_id(id: &str, archive_ids: Option<&[String]>) -> bool {
    match archive_ids {
        None | Some([]) => true,
        Some(ids) => ids.iter().any(|want| want == id),
    }
}

/// Whether an archive's `binaries:` allow-list packs a build entry's binary.
/// Mirrors the archive stage's own per-target filter, where an EMPTY list
/// selects nothing (unlike `ids:`, where it selects everything).
fn archive_packs_binary(binary: &str, archive_binaries: Option<&[String]>) -> bool {
    match archive_binaries {
        None => true,
        Some(names) => names.iter().any(|want| want == binary),
    }
}

/// Whether a build entry's `skip:` evaluates truthy. An expression that fails
/// to render does not skip the build — the lenient reading, shared with
/// preflight's `entry_inactive`; the build stage itself propagates such a
/// render error. THE spelling of that gate for callers supplying the
/// `is_skipped` predicate [`crate_build_target_entries`],
/// [`crate_target_list`] and [`crate_primary_binary_name`] take.
pub fn build_is_skipped(
    build: &BuildConfig,
    render: impl Fn(&str) -> anyhow::Result<String>,
) -> bool {
    try_build_is_skipped(build, render).unwrap_or(false)
}

/// Whether a build entry's `skip:` evaluates truthy, keeping a render error as
/// an error.
///
/// The planner surfaces a broken `skip:` expression instead of building the
/// entry it could not decide about; [`build_is_skipped`] is the lenient
/// reading of the same rule for callers that only need the predicate.
pub fn try_build_is_skipped(
    build: &BuildConfig,
    render: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    match build.skip.as_ref() {
        Some(s) => s.try_evaluates_to_true(render),
        None => Ok(false),
    }
}

/// [`build_is_skipped`] bound to a live context — the `skip:` gate with the
/// context's own template renderer already supplied.
///
/// Every consumer of the "which builds does THIS run release" question needs
/// the same adapter, so it is spelled here rather than at each call site:
/// pass the result straight to [`crate_primary_binary_name`],
/// [`crate_target_list`] or [`crate_build_target_entries`].
pub fn skipped_in(ctx: &Context) -> impl Fn(&BuildConfig) -> bool + '_ {
    move |build| build_is_skipped(build, |t| ctx.render_template(t))
}

/// [`crate_primary_binary_name`] resolved against a live context.
pub fn crate_primary_binary_name_in(ctx: &Context, krate: &CrateConfig) -> String {
    crate_primary_binary_name(krate, skipped_in(ctx))
}

/// [`crate_target_list`] resolved against a live context.
pub fn crate_target_list_in(
    ctx: &Context,
    krate: &CrateConfig,
    default_targets: &[String],
) -> Vec<String> {
    crate_target_list(krate, default_targets, skipped_in(ctx))
}

/// The binary an archive's assets are named after on one target — the value
/// bound to `{{ .Binary }}` while rendering its `name_template`.
///
/// The archive stage names each asset after the first binary the entry packs
/// FOR THAT TARGET, and it narrows its candidates four ways before picking it:
/// the entry's `ids:` filter, the per-target grouping (each build contributes
/// only the targets its own `targets:` names), the entry's `binaries:`
/// allow-list, and each build's `skip:`. A crate that splits its builds by
/// platform therefore has a different `{{ .Binary }}` per target. Every
/// derived-name consumer (the cargo-binstall `pkg_url`, the `curl | sh`
/// installer's asset table) must apply all four the same way or it publishes a
/// URL the release never uploaded. Falls back to the crate's own `[[bin]]`
/// name, which is what a build entry declaring no `binary:` compiles.
///
/// `render` resolves templated config values against the caller's live
/// context.
pub fn archive_binary_name(
    krate: &CrateConfig,
    archive_ids: Option<&[String]>,
    archive_binaries: Option<&[String]>,
    target: &str,
    default_targets: &[String],
    render: impl Fn(&str) -> anyhow::Result<String>,
) -> String {
    crate_build_target_entries(krate, default_targets, |build| {
        build_is_skipped(build, &render)
    })
    .into_iter()
    .find_map(|entry| {
        // `stage-build` renders the binary name, and the `binary`-fallback id
        // it derives from it, once per target before stamping either on the
        // artifact the archive stage then filters — so a `binary:` that is
        // itself a template is matched and displayed RENDERED. An explicit
        // `build.id` is stamped verbatim and must never be rendered.
        let binary = render(&entry.binary).unwrap_or(entry.binary);
        let id = match &entry.id {
            BuildId::Explicit(raw) => raw.clone(),
            BuildId::BinaryFallback(raw) => render(raw).unwrap_or_else(|_| raw.clone()),
        };
        (entry.targets.iter().any(|t| t == target)
            && archive_selects_id(&id, archive_ids)
            && archive_packs_binary(&binary, archive_binaries))
        .then_some(binary)
    })
    .unwrap_or_else(|| binary_or_crate_name(krate, &BuildConfig::default()))
}

/// The de-duplicated, order-preserving list of target triples a crate's builds
/// will actually produce: planner synthesis ([`planned_builds`]) + the compile/
/// artifact gate ([`build_produces`]) + `is_skipped` + per-build `targets:`
/// override of `default_targets`. THE single source of truth for crate target
/// enumeration.
///
/// `is_skipped` vetoes a build entry the same way it does in
/// [`crate_build_target_entries`] — pass [`build_is_skipped`] against a live
/// context to enumerate the triples THIS run releases, or `|_| false` to
/// enumerate every configured triple (what a config-time check wants, since a
/// `skip:` expression can resolve differently on the machine that releases).
pub fn crate_target_list(
    krate: &CrateConfig,
    default_targets: &[String],
    is_skipped: impl FnMut(&BuildConfig) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in crate_build_target_entries(krate, default_targets, is_skipped) {
        for t in entry.targets {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal crate skeleton with the given Cargo.toml + optional
    /// `src/main.rs` so the filesystem probes have something to read.
    fn crate_dir(cargo_toml: &str, with_main: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), cargo_toml).unwrap();
        if with_main {
            std::fs::create_dir_all(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        }
        dir
    }

    fn krate_at(name: &str, path: &str, builds: Option<Vec<BuildConfig>>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            builds,
            ..Default::default()
        }
    }

    /// The synthesized default build must name its binary through the same
    /// helper every other site derives a binary name from. Spelled a second
    /// time here, the synthesized entry stops moving when the fallback rule
    /// moves, and the name the planner compiles diverges from every name
    /// derived from it.
    #[test]
    fn the_synthesized_default_build_names_the_binary_the_helper_names() {
        let dir = crate_dir("[package]\nname = \"my_app\"\nversion = \"0.0.0\"\n", true);
        let krate = krate_at("my_app", dir.path().to_str().unwrap(), None);
        let builds = planned_builds(&krate).expect("a crate declaring its own bin plans a build");
        assert_eq!(
            builds.iter().map(|b| b.binary.clone()).collect::<Vec<_>>(),
            vec![Some(binary_or_crate_name(&krate, &BuildConfig::default()))],
        );
    }

    #[test]
    fn build_produces_false_for_binary_none_library_crate() {
        // Library crate (no src/main.rs, no [[bin]]) carrying a materialized
        // `binary: None` build — the planner skips it, so build_produces is false.
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig::default();
        assert!(!build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_prebuilt() {
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig {
            builder: Some(BuilderKind::Prebuilt),
            ..Default::default()
        };
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_explicit_binary() {
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig {
            binary: Some("app".to_string()),
            ..Default::default()
        };
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_declared_bin() {
        // src/main.rs + package name == crate name → declares a `--bin <crate>`.
        let dir = crate_dir("[package]\nname = \"app\"\nversion = \"0.0.0\"\n", true);
        let krate = krate_at("app", dir.path().to_str().unwrap(), None);
        let build = BuildConfig::default();
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn crate_target_list_empty_for_library_with_materialized_binary_none_build() {
        // A library crate that inherited a `defaults.builds` template carries a
        // build with `binary: None`; with no `--bin <crate>` target the gate
        // drops it, so the crate produces no targets.
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at(
            "lib",
            dir.path().to_str().unwrap(),
            Some(vec![BuildConfig::default()]),
        );
        let defaults = vec!["x86_64-unknown-linux-gnu".to_string()];
        assert!(crate_target_list(&krate, &defaults, |_| false).is_empty());
    }

    /// A build entry's `skip:` is evaluated in two shapes: leniently, where a
    /// render failure means "not skipped" ([`build_is_skipped`]), and
    /// strictly, where it is an error the caller propagates
    /// ([`try_build_is_skipped`]). Each shape has ONE spelling, and a
    /// hand-written copy of either is how a caller drifts from the planner it
    /// is supposed to mirror — which is what `cross_requirements` did until it
    /// routed here.
    ///
    /// The named owners each read `build.skip` for a reason that is not a
    /// second copy of a gate:
    ///
    /// | Owner | Why it reads the field directly |
    /// |---|---|
    /// | `try_build_is_skipped` | it IS the strict gate; the callers that propagate a render error (`build_skipped`, `plan_prebuilt_build`, `plan_build_jobs`) route through it |
    /// | `configured_build_targets` (`env_preflight.rs`) | hands the field to the shared `entry_inactive` predicate |
    #[test]
    fn every_build_skip_read_belongs_to_a_named_owner() {
        use crate::test_helpers::test_sources::{
            function_bodies, production_half, workspace_production_sources,
        };

        const OWNERS: &[&str] = &["try_build_is_skipped", "configured_build_targets"];

        let sources = workspace_production_sources();

        let mut strays: Vec<String> = Vec::new();
        for source in &sources {
            let text = std::fs::read_to_string(source).expect("read source");
            for body in function_bodies(production_half(&text)) {
                // Whitespace around `.` is erased so a receiver split across
                // lines reads the same as one written inline.
                let flat = body
                    .replace('\n', " ")
                    .split('.')
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(".");
                if !flat.contains("build.skip") {
                    continue;
                }
                let name = body
                    .lines()
                    .next()
                    .and_then(|l| l.split("fn ").nth(1))
                    .and_then(|l| l.split(['(', '<', ' ']).next())
                    .unwrap_or("<unnamed>")
                    .to_string();
                if !OWNERS.contains(&name.as_str()) {
                    strays.push(format!("{}: {name}", source.display()));
                }
            }
        }
        assert!(
            strays.is_empty(),
            "a build entry's skip: is read by one of {OWNERS:?}; these read it \
             themselves and will drift from the planner: {strays:#?}"
        );
    }

    /// The adapter that turns a live [`Context`] into the lenient `skip:` gate
    /// is [`skipped_in`], and nothing else: a hand-written
    /// `build_is_skipped(build, |t| ctx.render_template(t))` at a consumer is
    /// how "which builds does this run release" drifts from the planner it
    /// mirrors. Six consumers spelled it themselves before they routed here.
    /// The strict gate is exempt: every caller of [`try_build_is_skipped`]
    /// names the entry in its own error context, so there is nothing to share.
    #[test]
    fn the_context_skip_adapter_is_spelled_once() {
        use crate::test_helpers::test_sources::{
            function_bodies, production_half, workspace_production_sources,
        };

        let sources = workspace_production_sources();

        let mut strays: Vec<String> = Vec::new();
        for source in &sources {
            let text = std::fs::read_to_string(source).expect("read source");
            for body in function_bodies(production_half(&text)) {
                // `try_build_is_skipped` is the STRICT gate: each caller
                // wraps it in its own error context, so it has no single
                // context adapter and its call sites are not strays.
                let lenient_calls = body.matches("build_is_skipped(").count()
                    - body.matches("try_build_is_skipped(").count();
                if lenient_calls == 0 || !body.contains("render_template(") {
                    continue;
                }
                let name = body
                    .lines()
                    .next()
                    .and_then(|l| l.split("fn ").nth(1))
                    .and_then(|l| l.split(['(', '<', ' ']).next())
                    .unwrap_or("<unnamed>")
                    .to_string();
                if name != "skipped_in" {
                    strays.push(format!("{}: {name}", source.display()));
                }
            }
        }
        assert!(
            strays.is_empty(),
            "bind the skip gate to a context through `build_plan::skipped_in`, \
             not a local closure: {strays:#?}"
        );
    }

    /// The planner must not build an entry whose `skip:` it could not
    /// evaluate, so the strict gate keeps the render error the lenient gate
    /// reads as "not skipped".
    #[test]
    fn the_strict_skip_gate_keeps_a_render_error_the_lenient_one_swallows() {
        use crate::config::StringOrBool;

        let build = BuildConfig {
            skip: Some(StringOrBool::String("{{ missing_var }}".to_string())),
            ..Default::default()
        };
        let render = |_: &str| anyhow::bail!("render failed");

        let err = try_build_is_skipped(&build, render)
            .expect_err("a skip: that cannot render is an error, not a false");
        assert!(err.to_string().contains("render failed"), "got: {err}");
        assert!(!build_is_skipped(&build, render));
    }

    #[test]
    fn crate_target_list_uses_default_targets_for_declared_bin() {
        let dir = crate_dir("[package]\nname = \"app\"\nversion = \"0.0.0\"\n", true);
        let krate = krate_at("app", dir.path().to_str().unwrap(), None);
        let defaults = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        assert_eq!(crate_target_list(&krate, &defaults, |_| false), defaults);
    }
}
