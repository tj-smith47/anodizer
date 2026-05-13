pub mod artifactory;
pub mod aur;
pub mod aur_source;
pub mod cargo;
pub mod chocolatey;
pub mod cloudsmith;
pub mod dockerhub;
pub mod homebrew;
pub(crate) mod http_upload;
pub mod krew;
pub mod mcp;
pub mod nix;
pub mod preflight;
pub mod scoop;
pub mod upload;
pub(crate) mod util;
pub mod winget;

use anodizer_core::config::PublishConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::Result;

use artifactory::publish_to_artifactory;
use aur::publish_to_aur;
use aur_source::{publish_to_aur_source, publish_top_level_aur_sources};
use cargo::publish_to_cargo;
use chocolatey::publish_to_chocolatey;
use cloudsmith::publish_to_cloudsmith;
use dockerhub::publish_to_dockerhub;
use homebrew::{publish_to_homebrew, publish_top_level_homebrew_casks};
use krew::publish_to_krew;
use mcp::publish_to_mcp;
use nix::publish_to_nix;
use scoop::publish_to_scoop;
use upload::publish_to_upload;
use winget::publish_to_winget;

/// Collect crate names that match the selection filter and have a specific
/// publisher configured (as determined by the predicate `has_config`).
///
/// Walks the same crate universe as `cargo.rs::publish_to_cargo` —
/// `ctx.config.crates` plus every `ctx.config.workspaces[].crates` —
/// so a workspace-only crate carrying a non-cargo publisher block
/// (`homebrew:`, `scoop:`, `aur:`, ...) is dispatched alongside the
/// crates from the top-level list. Without this, cargo would publish
/// the workspace crate but every other publisher would silently skip
/// it. See `util::all_crates` for the dedup rule.
fn crates_with_publisher<F>(ctx: &Context, selected: &[String], has_config: F) -> Vec<String>
where
    F: Fn(&PublishConfig) -> bool,
{
    util::all_crates(ctx)
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.publish.as_ref().is_some_and(&has_config))
        .map(|c| c.name)
        .collect()
}

/// Route a single publisher's `Result` through the stage's collect-or-bail
/// policy. Returns `Ok(())` for the caller to continue dispatching the
/// remaining publishers; returns `Err(...)` only when `fail_fast` is on and
/// the publisher failed — at which point the enclosing stage's `?` exits
/// immediately, matching GoReleaser's `--fail-fast` semantics in
/// `internal/pipe/publish/publish.go`.
///
/// On a publisher failure with `fail_fast == false` (the default), the error
/// is logged and pushed to `errors` for end-of-stage aggregation. This is the
/// "continue-on-error" path that mirrors GoReleaser's `Continuable`
/// publishers (brew, krew, nix, scoop, winget, cask, aur, chocolatey, ...).
fn record_publisher_result(
    label: &str,
    result: Result<()>,
    fail_fast: bool,
    errors: &mut Vec<String>,
    log: &StageLogger,
) -> Result<()> {
    if let Err(e) = result {
        // `{:#}` renders the full anyhow error chain on one line
        // (e.g. "top: middle: root cause"). `{}` shows only the
        // top context, which discards the actual root cause —
        // hiding details like reqwest transport errors, HTTP
        // status codes, or response bodies that operators need
        // to diagnose a failing publisher.
        let formatted = format!("{}: {:#}", label, e);
        log.warn(&formatted);
        if fail_fast {
            anyhow::bail!("publisher failed (fail-fast): {}", formatted);
        }
        errors.push(formatted);
    }
    Ok(())
}

pub struct PublishStage;

impl Stage for PublishStage {
    fn name(&self) -> &str {
        "publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        if ctx.skip_in_snapshot(&log, "publish") {
            return Ok(());
        }
        let selected = ctx.options.selected_crates.clone();
        // Capture as a local so the macros below can read it without
        // re-borrowing `ctx` mid-dispatch (every publisher call takes
        // `&mut Context` indirectly via stage hand-off).
        let fail_fast = ctx.options.fail_fast;

        // Individual publisher failures are collected and reported at the end
        // rather than aborting the entire publish stage. This prevents a single
        // publisher (e.g. homebrew auth) from killing independent downstream
        // publishers (docker, cosign, announce). crates.io is the exception —
        // it's the authoritative registry and its failure is always fatal.
        //
        // `--fail-fast` inverts this: the first publisher error aborts the
        // stage immediately (see `record_publisher_result`). Default
        // collect-and-aggregate matches GoReleaser's `Continuable` post-
        // release publishers; fail-fast matches `internal/pipe/publish/
        // publish.go:95` upstream when `ctx.FailFast` is on.
        //
        // Strict mode semantics: we still COLLECT every publisher error so a
        // single run surfaces *all* remaining issues. The difference vs. the
        // default mode is that at the end of the stage we bail with the full
        // list instead of warning. Failing fast on the first error is
        // counter-productive for dogfooding — it hides every issue after the
        // first, forcing N release cycles to shake out N bugs — which is
        // exactly why fail-fast is opt-in.
        let mut errors: Vec<String> = Vec::new();

        // Helper: run a publisher, log + collect (default) or bail (fail-fast)
        // on failure. Routes through `record_publisher_result` so the policy
        // stays unit-testable; the `?` propagates a fail-fast bail out of the
        // enclosing `run`.
        macro_rules! try_publish {
            ($label:expr, $expr:expr) => {
                record_publisher_result($label, $expr, fail_fast, &mut errors, &log)?;
            };
        }

        // infra-level publishers (blob,
        // upload, artifactory, docker-signs, snapcraft/dockerhub) run BEFORE
        // package managers (homebrew/cask/scoop/chocolatey/winget/aur/krew/nix).
        // Package managers often reference release artifacts by URL+digest, so
        // those URLs must be live before the manifests are published.
        //
        // crates.io is dispatched first (after the macro definitions below)
        // and is fatal — it's the authoritative Rust registry and must
        // succeed before anything downstream runs. `aur_source`/`aur_sources`
        // run last to match GoReleaser.

        // ---- Infrastructure publishers (run before package managers) ----

        // 2. DockerHub — top-level publisher (not per-crate).
        try_publish!("dockerhub", publish_to_dockerhub(ctx, &log));

        // 3. Artifactory — top-level publisher (not per-crate).
        try_publish!("artifactory", publish_to_artifactory(ctx, &log));

        // 4. CloudSmith — top-level publisher (not per-crate).
        try_publish!("cloudsmith", publish_to_cloudsmith(ctx, &log));

        // 5. Generic HTTP upload — top-level publisher.
        try_publish!("upload", publish_to_upload(ctx, &log));

        // ---- Package-manager publishers (consume URLs from releases above) ----
        //
        // Every entry below is dispatched through one of two macros so the
        // skip gate, log line, and label are produced uniformly:
        //
        //   per_crate!  — fan out per `selected` crate that has the publisher
        //                  configured. Predicate filters `PublishConfig`.
        //   top_level!  — single top-level call (no per-crate fan-out).
        //
        // Skip names match GoReleaser convention: `brew`, `scoop`, `choco`,
        // `winget`, `aur`, `krew`, `nix`, `cargo`. The skip gate fires from
        // here for every publisher (cargo included) so the user sees a single
        // uniform "X: skipped via --skip=X" line regardless of which publisher
        // owns the actual subprocess. `--skip=brew` and `--skip=aur` each gate
        // two related sub-publishers (formula+casks, binary+source).

        // Dispatcher helpers — collapse per-publisher boilerplate.
        // Each macro:
        //   1. checks `ctx.should_skip($skip_name)`,
        //   2. emits "{label}: skipped via --skip={skip_name}" if skipped,
        //   3. otherwise runs the publisher and routes errors through
        //      `try_publish!` (collected for end-of-stage aggregation).
        macro_rules! per_crate {
            ($skip:expr, $label:expr, $pred:expr, $run:expr) => {{
                if ctx.should_skip($skip) {
                    log.status(&format!("{}: skipped via --skip={}", $label, $skip));
                } else {
                    for crate_name in &crates_with_publisher(ctx, &selected, $pred) {
                        try_publish!($label, $run(ctx, crate_name, &log));
                    }
                }
            }};
        }
        macro_rules! top_level {
            ($skip:expr, $label:expr, $run:expr) => {{
                if ctx.should_skip($skip) {
                    log.status(&format!("{}: skipped via --skip={}", $label, $skip));
                } else {
                    try_publish!($label, $run(ctx, &log));
                }
            }};
        }

        // Cargo (crates.io) — top-level by virtue of doing its own crate
        // walk + topo sort internally. Fatal regardless of `--fail-fast`:
        // any error aborts the stage because crates.io is the authoritative
        // Rust registry and downstream publishers reference its URLs. The
        // `?` below intentionally bypasses `record_publisher_result`.
        if ctx.should_skip("cargo") {
            log.status("cargo: skipped via --skip=cargo");
        } else {
            publish_to_cargo(ctx, &selected, &log)?;
        }

        // 7b. MCP server registry — top-level publisher. Posts an
        // apiv0.ServerJSON document to the configured MCP registry. Skipped
        // when `mcp.name` is empty (same gate GoReleaser uses in its `mcp`
        // pipe).
        top_level!("mcp", "mcp", publish_to_mcp);

        // 8. Homebrew formulae — per-crate.
        per_crate!(
            "brew",
            "homebrew",
            |p: &PublishConfig| p.homebrew.is_some(),
            publish_to_homebrew
        );

        // 9. Scoop — per-crate.
        per_crate!(
            "scoop",
            "scoop",
            |p: &PublishConfig| p.scoop.is_some(),
            publish_to_scoop
        );

        // 10. Chocolatey — per-crate.
        per_crate!(
            "choco",
            "chocolatey",
            |p: &PublishConfig| p.chocolatey.is_some(),
            publish_to_chocolatey
        );

        // 11. WinGet — per-crate.
        per_crate!(
            "winget",
            "winget",
            |p: &PublishConfig| p.winget.is_some(),
            publish_to_winget
        );

        // 12. AUR (binary) — per-crate. Shares `--skip=aur` with aur-source.
        per_crate!(
            "aur",
            "aur",
            |p: &PublishConfig| p.aur.is_some(),
            publish_to_aur
        );

        // 13. Krew — per-crate.
        per_crate!(
            "krew",
            "krew",
            |p: &PublishConfig| p.krew.is_some(),
            publish_to_krew
        );

        // 14. Nix — per-crate.
        per_crate!(
            "nix",
            "nix",
            |p: &PublishConfig| p.nix.is_some(),
            publish_to_nix
        );

        // 15. Homebrew Casks — top-level publisher (GoReleaser parity).
        // Shares `--skip=brew` with the per-crate formula publisher above; the
        // skip emits twice (once for "homebrew", once for "homebrew-casks") so
        // operators see exactly which surface was suppressed.
        top_level!("brew", "homebrew-casks", publish_top_level_homebrew_casks);

        // ---- AUR source last (GoReleaser parity) ----

        // 16. AUR source packages — per-crate publisher.
        per_crate!(
            "aur",
            "aur-source",
            |p: &PublishConfig| p.aur_source.is_some(),
            publish_to_aur_source
        );

        // 17. AUR source packages — top-level array (GoReleaser `aur_sources`).
        top_level!("aur", "aur-sources", publish_top_level_aur_sources);

        if errors.is_empty() {
            Ok(())
        } else {
            let suffix = if ctx.is_strict() {
                " (strict mode)"
            } else {
                ""
            };
            anyhow::bail!(
                "{} publisher(s) failed{}:\n  {}",
                errors.len(),
                suffix,
                errors.join("\n  ")
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        AurConfig, CargoPublishConfig, Config, CrateConfig, HomebrewConfig, PublishConfig,
        WorkspaceConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_name() {
        assert_eq!(PublishStage.name(), "publish");
    }

    #[test]
    fn test_run_no_crates_configured() {
        let config = Config::default();
        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    /// WAVE 3: a workspace-only crate that carries a non-cargo publisher block
    /// (homebrew/scoop/aur/...) must be visible to `crates_with_publisher`,
    /// matching the universe `cargo.rs::publish_to_cargo` walks. Before the
    /// shared `util::all_crates` lift, this crate would silently disappear
    /// from every non-cargo dispatcher even though cargo would still publish it.
    #[test]
    fn test_crates_with_publisher_includes_workspace_only_crates() {
        let mut config = Config::default();
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "ws-only".to_string(),
                path: "crates/ws-only".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(names, vec!["ws-only".to_string()]);
    }

    /// WAVE 3 dedup rule: top-level `crates` wins on name collision with a
    /// workspace entry. Both walkers (cargo + non-cargo) must see exactly
    /// one entry per name so `expand_with_transitive_deps` and the
    /// publisher loops never double-publish.
    #[test]
    fn test_crates_with_publisher_dedupes_top_level_over_workspace() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "shared".to_string(),
            path: "top".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                // Same name as the top-level — top-level must win.
                name: "shared".to_string(),
                path: "ws/shared".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: None,
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(
            names,
            vec!["shared".to_string()],
            "top-level entry must win on name collision and not be doubled"
        );
    }

    #[test]
    fn test_run_dry_run_cargo() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // dry-run: should log but not actually shell out
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_publish_config_is_noop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "nopub".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: None, // No publish config
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // Should succeed (no-op)
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    /// Document current behavior: the publish stage does NOT skip homebrew/scoop
    /// publishing for prerelease versions. It proceeds regardless of whether
    /// the version contains a prerelease suffix like -rc.1 or -beta.
    ///
    /// This is a known limitation: GoReleaser skips homebrew/scoop for prereleases
    /// by default. If this behavior is added in the future, this test should be
    /// updated to verify that skipping occurs.
    // -----------------------------------------------------------------------
    // Chocolatey integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // WinGet integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // AUR integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_aur() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Krew integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Top-level AUR sources integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_top_level_aur_sources() {
        use anodizer_core::config::AurSourceConfig;

        let mut config = Config::default();
        config.aur_sources = Some(vec![AurSourceConfig {
            name: Some("myapp".to_string()),
            description: Some("My application".to_string()),
            license: Some("MIT".to_string()),
            git_url: Some("ssh://aur@aur.archlinux.org/myapp.git".to_string()),
            makedepends: Some(vec!["rust".to_string(), "cargo".to_string()]),
            ..Default::default()
        }]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_empty_is_noop() {
        let mut config = Config::default();
        config.aur_sources = Some(vec![]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_none_is_noop() {
        let mut config = Config::default();
        config.aur_sources = None;

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Nix integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // record_publisher_result — fail_fast wiring
    //
    // These tests pin the collect-or-bail policy that the publish stage
    // dispatch macros route every publisher through. The default is
    // collect-and-aggregate (matches GoReleaser's `Continuable` publishers);
    // `--fail-fast` inverts it so the very first publisher error aborts the
    // stage immediately (matches `internal/pipe/publish/publish.go:95`
    // upstream).
    // -----------------------------------------------------------------------

    use anodizer_core::log::{StageLogger, Verbosity};

    fn test_logger() -> StageLogger {
        StageLogger::new("publish-test", Verbosity::Quiet)
    }

    #[test]
    fn test_record_publisher_result_ok_is_noop() {
        let log = test_logger();
        let mut errors: Vec<String> = Vec::new();
        let res = record_publisher_result("homebrew", Ok(()), false, &mut errors, &log);
        assert!(res.is_ok());
        assert!(errors.is_empty(), "no failures => errors stays empty");

        let res = record_publisher_result("homebrew", Ok(()), true, &mut errors, &log);
        assert!(res.is_ok());
        assert!(errors.is_empty(), "fail_fast on Ok still empty");
    }

    #[test]
    fn test_record_publisher_result_default_collects() {
        // Default mode (fail_fast=false): two consecutive publisher failures
        // both end up in `errors` and the helper returns Ok(()) each time so
        // the dispatch loop continues.
        let log = test_logger();
        let mut errors: Vec<String> = Vec::new();

        let res = record_publisher_result(
            "homebrew",
            Err(anyhow::anyhow!("tap repo not found")),
            false,
            &mut errors,
            &log,
        );
        assert!(res.is_ok(), "default mode never short-circuits");

        let res = record_publisher_result(
            "scoop",
            Err(anyhow::anyhow!("bucket auth failed")),
            false,
            &mut errors,
            &log,
        );
        assert!(res.is_ok(), "default mode never short-circuits");

        assert_eq!(errors.len(), 2, "both failures collected");
        assert!(errors[0].starts_with("homebrew: "));
        assert!(errors[0].contains("tap repo not found"));
        assert!(errors[1].starts_with("scoop: "));
        assert!(errors[1].contains("bucket auth failed"));
    }

    #[test]
    fn test_record_publisher_result_fail_fast_bails_on_first() {
        // fail_fast mode: the first publisher failure returns Err so the
        // enclosing stage's `?` exits the run immediately. The second
        // publisher must not be invoked, and `errors` must NOT contain the
        // first failure (it's surfaced via the bail!, not the aggregate).
        let log = test_logger();
        let mut errors: Vec<String> = Vec::new();

        let res = record_publisher_result(
            "homebrew",
            Err(anyhow::anyhow!("tap repo not found")),
            true,
            &mut errors,
            &log,
        );
        let err = match res {
            Ok(()) => panic!("fail_fast must short-circuit on first error"),
            Err(e) => e,
        };
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("fail-fast"),
            "error message should signal fail-fast trigger, got: {msg}"
        );
        assert!(
            msg.contains("homebrew"),
            "error message should name the failing publisher, got: {msg}"
        );
        assert!(
            msg.contains("tap repo not found"),
            "error message should preserve the underlying cause, got: {msg}"
        );
        assert!(
            errors.is_empty(),
            "fail_fast surfaces the error via Err, not via the aggregate vec; got {errors:?}"
        );
    }

    #[test]
    fn test_run_dry_run_nix() {
        use anodizer_core::config::{NixConfig, RepositoryConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("myorg".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }
}
