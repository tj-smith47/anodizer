//! Cross-toolchain self-report for the `anodizer tools` introspection command.
//!
//! The build stage resolves its cross-compilation strategy (cargo-zigbuild,
//! `cross`, or plain cargo) per target at RUNTIME, so a static
//! `Tool { cargo }` requirement under-reports what a runner actually needs to
//! cross-compile. The GitHub Action consumes `anodizer tools` to decide what to
//! install; without the resolved cross toolchain it re-derives those deps in
//! bash and drifts from anodizer's own routing. [`cross_tool_requirements`]
//! emits the cross toolchain (`cargo-zigbuild` + `zig`, `cross`, or the system
//! `{arch}-linux-gnu-gcc`) so the install hint matches what the build will run.

use std::collections::BTreeSet;

use anodizer_core::EnvRequirement;
use anodizer_core::config::{BuilderKind, CrossStrategy};
use anodizer_core::context::Context;

use crate::command::{cross_gnu_cargo_gcc, detect_cross_strategy_for_target_impl, planned_builds};
use crate::targets::is_target_ignored;

/// Derive the cross-compilation toolchain every configured build target
/// requires, as install hints for `anodizer tools`.
///
/// For each compiled build entry across `crates` and `workspaces[].crates`
/// (enumerated through the shared [`planned_builds`] SSOT so this hint matches
/// what the planner compiles — per-build `targets` override `defaults.targets`,
/// and `ignore` os/arch pairs are filtered out), the crate's `cross:` strategy
/// (`auto` when unset) is resolved per target and mapped to its toolchain:
///
/// - `zigbuild` → `cargo-zigbuild` AND `zig` (zigbuild shells out to zig)
/// - `cross` → `cross`
/// - `cargo` → the system `{arch}-linux-gnu-gcc` for a cross-arch `*-linux-gnu`
///   target (native / apple-family / windows-family cargo needs no extra tool)
///
/// A per-build `cross_tool` override names the binary literally and wins over
/// the strategy. `builder: prebuilt` entries and `skip`-truthy entries import
/// or omit a binary with no compilation, so they contribute nothing.
///
/// Strategy resolution uses the assume-available form (zig + cross both
/// present): this is an install HINT, not a probe of the current PATH — the
/// runner has not installed anything yet, so resolving against the live PATH
/// would always collapse to the plain-cargo fallback and emit nothing.
///
/// Tools are deduped into a stable, sorted set. `cargo` itself is NOT emitted
/// here — the preflight build block already declares it.
pub fn cross_tool_requirements(ctx: &Context) -> Vec<EnvRequirement> {
    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    let selected = &ctx.options.selected_crates;

    // SSOT for the target-set and strategy fallbacks: resolve through the same
    // `Config` helpers the build planner uses so this hint cannot drift from
    // what the build actually compiles.
    let default_targets = ctx.config.effective_default_targets();
    let default_strategy = ctx.config.default_cross_strategy();
    let default_ignore = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.builds.as_ref())
        .and_then(|b| b.ignore.clone())
        .unwrap_or_default();

    let all_crates = ctx.config.crates.iter().chain(
        ctx.config
            .workspaces
            .as_deref()
            .unwrap_or_default()
            .iter()
            .flat_map(|w| w.crates.iter()),
    );

    let mut tools: BTreeSet<String> = BTreeSet::new();

    for krate in all_crates {
        if !selected.is_empty() && !selected.contains(&krate.name) {
            continue;
        }
        let strategy = krate
            .cross
            .clone()
            .unwrap_or_else(|| default_strategy.clone());

        // Enumerate exactly what the build planner will compile for this crate
        // (non-empty `builds:` as-is, else a synthesized default `--bin <name>`
        // build, else nothing) via the shared SSOT so this hint cannot drift.
        let Some(builds) = planned_builds(krate) else {
            continue;
        };

        for build in &builds {
            // Prebuilt imports a staged binary; nothing is compiled.
            if matches!(build.builder, Some(BuilderKind::Prebuilt)) {
                continue;
            }
            // A skipped build runs no cargo invocation. Render failures fall
            // back to "not skipped" so the hint over-reports rather than
            // silently dropping a toolchain a real build would need.
            let skipped = build
                .skip
                .as_ref()
                .map(|s| {
                    s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if skipped {
                continue;
            }

            // Per-build targets REPLACE defaults.targets; per-build ignore
            // falls back to defaults.builds.ignore (mirrors the build stage).
            // An explicitly empty `targets: []` means "skip this build".
            let chosen: &[String] = match build.targets.as_deref() {
                Some(ts) => ts,
                None => default_targets.as_slice(),
            };
            let build_ignore = build
                .ignore
                .clone()
                .unwrap_or_else(|| default_ignore.clone());

            // The targets that survive the ignore filter are the ones the build
            // actually compiles. If none survive, the build runs nothing, so it
            // contributes no toolchain — a `cross_tool` override included.
            let live: Vec<&String> = chosen
                .iter()
                .filter(|t| !is_target_ignored(t.as_str(), &build_ignore))
                .collect();
            if live.is_empty() {
                continue;
            }

            // An explicit cross_tool names the binary verbatim and overrides
            // strategy resolution entirely.
            if let Some(tool) = build.cross_tool.as_deref().filter(|s| !s.is_empty()) {
                tools.insert(tool.to_string());
                continue;
            }

            for target in live {
                let resolved = if strategy == CrossStrategy::Auto {
                    detect_cross_strategy_for_target_impl(&host, target, true, true)
                } else {
                    strategy.clone()
                };
                match resolved {
                    CrossStrategy::Zigbuild => {
                        tools.insert("cargo-zigbuild".to_string());
                        tools.insert("zig".to_string());
                    }
                    CrossStrategy::Cross => {
                        tools.insert("cross".to_string());
                    }
                    CrossStrategy::Cargo => {
                        if let Some(gcc) = cross_gnu_cargo_gcc(&host, target) {
                            tools.insert(gcc);
                        }
                    }
                    // Auto is always resolved to a concrete strategy above.
                    CrossStrategy::Auto => {}
                }
            }
        }
    }

    tools
        .into_iter()
        .map(|name| EnvRequirement::Tool { name })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{BuildConfig, BuildIgnore, CrateConfig, Defaults, StringOrBool};
    use anodizer_core::test_helpers::TestContextBuilder;

    const HOST: &str = "x86_64-unknown-linux-gnu";

    /// A crate with one build over `targets`, the given `cross:` strategy, and
    /// optional `cross_tool` / `builder` / `skip`.
    fn krate(name: &str, cross: Option<CrossStrategy>, build: BuildConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            cross,
            builds: Some(vec![build]),
            ..Default::default()
        }
    }

    fn build_for(targets: &[&str]) -> BuildConfig {
        BuildConfig {
            binary: Some("app".to_string()),
            targets: Some(targets.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn tool_names(reqs: &[EnvRequirement]) -> Vec<String> {
        reqs.iter()
            .map(|r| match r {
                EnvRequirement::Tool { name } => name.clone(),
                other => panic!("cross_tool_requirements emitted a non-Tool req: {other:?}"),
            })
            .collect()
    }

    fn run(krates: Vec<CrateConfig>) -> Vec<String> {
        let ctx = TestContextBuilder::new().crates(krates).build();
        tool_names(&cross_tool_requirements(&ctx))
    }

    // The strategy-detector core is reused, so its host/target routing is the
    // contract under test here. On a non-linux-gnu host the Auto cases would
    // route differently; pin the host the assertions assume.
    fn host_is_x86_64_linux_gnu() -> bool {
        anodizer_core::partial::detect_host_target()
            .as_deref()
            .unwrap_or_default()
            == HOST
    }

    #[test]
    fn auto_cross_arch_linux_gnu_emits_zigbuild_and_zig() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        let names = run(vec![krate(
            "c",
            Some(CrossStrategy::Auto),
            build_for(&["aarch64-unknown-linux-gnu"]),
        )]);
        assert_eq!(names, vec!["cargo-zigbuild", "zig"]);
    }

    #[test]
    fn auto_linux_musl_emits_zigbuild_and_zig() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        let names = run(vec![krate(
            "c",
            Some(CrossStrategy::Auto),
            build_for(&["aarch64-unknown-linux-musl"]),
        )]);
        assert_eq!(names, vec!["cargo-zigbuild", "zig"]);
    }

    #[test]
    fn explicit_cross_strategy_emits_cross() {
        let names = run(vec![krate(
            "c",
            Some(CrossStrategy::Cross),
            build_for(&["aarch64-unknown-linux-gnu"]),
        )]);
        assert_eq!(names, vec!["cross"]);
    }

    #[test]
    fn explicit_cargo_cross_arch_gnu_emits_system_gcc() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        let names = run(vec![krate(
            "c",
            Some(CrossStrategy::Cargo),
            build_for(&["aarch64-unknown-linux-gnu"]),
        )]);
        assert_eq!(names, vec!["aarch64-linux-gnu-gcc"]);
    }

    #[test]
    fn native_host_target_emits_nothing() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        // Explicit Cargo on the host triple: native build, no cross gcc.
        let names = run(vec![krate(
            "c",
            Some(CrossStrategy::Cargo),
            build_for(&[HOST]),
        )]);
        assert!(
            names.is_empty(),
            "host-triple cargo needs no extra tool: {names:?}"
        );
    }

    #[test]
    fn prebuilt_build_emits_nothing() {
        let mut build = build_for(&["aarch64-unknown-linux-gnu"]);
        build.builder = Some(BuilderKind::Prebuilt);
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert!(
            names.is_empty(),
            "prebuilt imports a binary, no toolchain: {names:?}"
        );
    }

    #[test]
    fn skipped_build_emits_nothing() {
        let mut build = build_for(&["aarch64-unknown-linux-gnu"]);
        build.skip = Some(StringOrBool::Bool(true));
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert!(names.is_empty(), "skipped build runs no cargo: {names:?}");
    }

    #[test]
    fn cross_tool_override_wins_over_strategy() {
        let mut build = build_for(&["aarch64-unknown-linux-gnu"]);
        build.cross_tool = Some("mycross".to_string());
        // Strategy says zigbuild; the literal cross_tool overrides it.
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert_eq!(names, vec!["mycross"]);
    }

    #[test]
    fn ignored_target_is_filtered() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        let mut build = build_for(&["aarch64-unknown-linux-gnu"]);
        // arm64/linux is the (os, arch) for aarch64-unknown-linux-gnu.
        build.ignore = Some(vec![BuildIgnore {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
        }]);
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert!(
            names.is_empty(),
            "ignored target must not emit tools: {names:?}"
        );
    }

    #[test]
    fn defaults_targets_are_inherited_when_build_targets_unset() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        let build = BuildConfig {
            binary: Some("app".to_string()),
            ..Default::default()
        };
        let defaults = Defaults {
            targets: Some(vec!["aarch64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![krate("c", Some(CrossStrategy::Auto), build)])
            .defaults(defaults)
            .build();
        let names = tool_names(&cross_tool_requirements(&ctx));
        assert_eq!(names, vec!["cargo-zigbuild", "zig"]);
    }

    #[test]
    fn output_is_deduped_and_sorted() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        // Two crates both cross to aarch64-gnu via Auto → one deduped pair.
        let names = run(vec![
            krate(
                "a",
                Some(CrossStrategy::Auto),
                build_for(&["aarch64-unknown-linux-gnu"]),
            ),
            krate(
                "b",
                Some(CrossStrategy::Auto),
                build_for(&["aarch64-unknown-linux-gnu"]),
            ),
        ]);
        assert_eq!(names, vec!["cargo-zigbuild", "zig"]);
    }

    #[test]
    fn no_default_targets_falls_back_to_default_set() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        // No defaults.targets and a build with targets:None resolves to the
        // canonical DEFAULT_TARGETS, which include an aarch64 gnu target →
        // cargo-zigbuild + zig. Regression guard: an empty `&[]` fallback
        // silently emitted nothing for exactly this aarch64 cross case.
        let build = BuildConfig {
            binary: Some("app".to_string()),
            ..Default::default()
        };
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert!(
            names.contains(&"cargo-zigbuild".to_string()) && names.contains(&"zig".to_string()),
            "DEFAULT_TARGETS include an aarch64 gnu target needing zigbuild: {names:?}"
        );
    }

    #[test]
    fn defaults_cross_strategy_applies_when_crate_omits_cross() {
        // defaults.cross = cross with a crate that sets no `cross:` → the
        // `cross` strategy applies and its binary is reported. Explicit
        // strategy bypasses host/target routing, so no host gate is needed.
        let defaults = Defaults {
            cross: Some(CrossStrategy::Cross),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![krate(
                "c",
                None,
                build_for(&["aarch64-unknown-linux-gnu"]),
            )])
            .defaults(defaults)
            .build();
        let names = tool_names(&cross_tool_requirements(&ctx));
        assert_eq!(names, vec!["cross"]);
    }

    #[test]
    fn crate_without_builds_but_declaring_bin_synthesizes_default_build() {
        if !host_is_x86_64_linux_gnu() {
            return;
        }
        // A crate with no `builds:` but a `--bin <name>` target compiles a
        // synthesized default build over DEFAULT_TARGETS (mirrors the planner).
        // crate_declares_bin reads the filesystem, so a real fixture is needed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"c\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let crate_cfg = CrateConfig {
            name: "c".to_string(),
            path: dir.path().to_string_lossy().into_owned(),
            builds: None,
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let names = tool_names(&cross_tool_requirements(&ctx));
        assert!(
            names.contains(&"cargo-zigbuild".to_string()),
            "synthesized default build over DEFAULT_TARGETS must report the cross toolchain: {names:?}"
        );
    }

    #[test]
    fn cross_tool_with_all_targets_ignored_emits_nothing() {
        // Every target ignored → the build runs nothing, so even a literal
        // cross_tool override contributes no toolchain.
        let mut build = build_for(&["aarch64-unknown-linux-gnu"]);
        build.cross_tool = Some("mycross".to_string());
        build.ignore = Some(vec![BuildIgnore {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
        }]);
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert!(
            names.is_empty(),
            "cross_tool must not emit when every target is ignored: {names:?}"
        );
    }

    #[test]
    fn cross_tool_emits_when_a_live_target_remains() {
        // Only the aarch64 leg is ignored; the x86_64 leg still builds, so the
        // cross_tool binary is reported once.
        let mut build = build_for(&["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]);
        build.cross_tool = Some("mycross".to_string());
        build.ignore = Some(vec![BuildIgnore {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
        }]);
        let names = run(vec![krate("c", Some(CrossStrategy::Auto), build)]);
        assert_eq!(names, vec!["mycross"]);
    }
}
