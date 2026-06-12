//! Pre-publish template-render guard.
//!
//! The release pipeline fires irreversible publishers — chocolatey/winget
//! moderation submissions, AUR pushes — and then renders `{{ ReleaseURL }}`-class
//! announce templates AFTER they have run. A publisher manifest or announce
//! template that fails to render (an undefined variable, a malformed Tera
//! expression) historically blew up only after a one-way door had already
//! fired, leaving a half-published release no rollback could undo.
//!
//! [`PrePublishGuardStage`] sits immediately after the release is created and
//! before the first publisher: it renders every configured publisher manifest
//! and every enabled announcer's templates in-memory — sending nothing, writing
//! nothing, reading no credentials — and aborts the release loud, listing every
//! broken template across publishers AND announcers, BEFORE any irreversible
//! publisher fires.

use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::crate_scope::resolve_crate_tag;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::{Result, bail};

/// Orchestrator stage that fails a release before any irreversible publisher
/// fires if a publisher-manifest or announcer template cannot render.
///
/// A no-op in snapshot / nightly (where no publisher or announcer dispatches),
/// so the guard never spuriously fails a mode in which nothing publishes.
pub struct PrePublishGuardStage;

impl Stage for PrePublishGuardStage {
    fn name(&self) -> &str {
        "prepublish-guard"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        // Real releases tag every selected crate, so the per-crate render scope
        // resolves each crate's own version via the git-backed resolver — the
        // same one the live publish path uses, correct in single-crate,
        // workspace-lockstep, and workspace per-crate modes.
        run_guard(ctx, &resolve_crate_tag)
    }
}

/// Resolver-injected guard body. Production passes
/// [`resolve_crate_tag`](anodizer_core::crate_scope::resolve_crate_tag)
/// (git-backed); tests inject a closure that derives each crate's version
/// without a git fixture, exactly as `stage-publish`'s schema validators do.
fn run_guard(
    ctx: &mut Context,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    let log = ctx.logger("prepublish-guard");

    // Match the modes under which one-way publishers + announcers actually
    // run. Snapshot and nightly dispatch neither, so a broken template there is
    // not a release-blocking defect — and emission/announce validation already
    // runs in the snapshot/dry-run emission-validate pass. Guarding only the
    // real-publish path keeps this gate honest.
    if ctx.is_snapshot() {
        log.status("skipping prepublish guard (snapshot mode)");
        return Ok(());
    }
    if ctx.is_nightly() {
        log.status("skipping prepublish guard (nightly run)");
        return Ok(());
    }

    guard_checks(ctx, &log, resolve_tag)
}

/// Run BOTH checks independently and accumulate every failure, so one run
/// surfaces the operator's full breakage list (publishers AND announcers)
/// rather than only the first.
fn guard_checks(
    ctx: &mut Context,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    let mut errors: Vec<String> = vec![];

    // Render strictly for the duration of the guard's in-memory pass: every
    // publisher/announce template the validators render must PROPAGATE a
    // malformed-template error (so it lands in `errors` and aborts the
    // release) instead of being swallowed into a warn + raw fallback.
    // Production publish renders stay lenient unless the user passed the
    // global `--strict`. Both checks accumulate their `Err` into `errors`
    // (never `?`-return), so the prior value is unconditionally restored
    // before this fn returns.
    let prior_strict = ctx.set_render_strict(true);

    // Guard only what will actually fire. A skipped stage publishes nothing, so
    // a template it would have rendered is not a release-blocking defect — and
    // some of those templates reference release-phase vars (`{{ ReleaseURL }}`)
    // that are only set once the `release` stage runs. The determinism harness
    // rebuilds with `--skip=release,publish,announce,...` (and NOT `--snapshot`
    // on a tag-push run, so `is_snapshot()` is false here): gating each check on
    // its stage keeps the guard a clean no-op there while still firing on a real
    // release where none of these are skipped and `ReleaseURL` is populated.
    if !ctx.should_skip("publish")
        && let Err(e) = anodizer_stage_publish::validate_publisher_schemas(ctx, log, resolve_tag)
    {
        errors.push(format!("{e:#}"));
    }

    if !ctx.should_skip("announce")
        && let Err(e) = anodizer_stage_announce::validate_announce_templates(ctx, log)
    {
        errors.push(format!("{e:#}"));
    }

    ctx.set_render_strict(prior_strict);

    if !errors.is_empty() {
        bail!(
            "prepublish-guard: template render failed before any publisher fired:\n{}",
            errors.join("\n")
        );
    }

    log.status("all publisher + announce templates render");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        AnnounceConfig, Config, CrateConfig, DiscordAnnounce, PublishConfig, ReleaseConfig,
        RepositoryConfig, ScmRepoConfig, ScoopConfig, StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn enabled() -> Option<StringOrBool> {
        Some(StringOrBool::Bool(true))
    }

    /// Test-only per-crate tag resolver: returns the version currently scoped on
    /// `ctx`, so the per-crate render scope re-derives the SAME version the test
    /// pre-set — exercising the publisher render path without a git fixture, the
    /// same shape `stage-publish`'s `test_current_version_resolver` uses.
    fn test_version_resolver() -> impl Fn(&Context, &CrateConfig) -> Option<String> {
        |ctx: &Context, _: &CrateConfig| {
            let v = ctx.version();
            (!v.trim().is_empty()).then_some(v)
        }
    }

    /// A scoped context whose `Version`/`Tag`/`ReleaseURL` are set the way a
    /// real release stamps them after `ReleaseStage`, so the per-crate render
    /// resolver (test-mode: derives the scoped version) lands a real version.
    fn scope(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
        ctx.set_release_url(&format!(
            "https://github.com/acme/widget/releases/tag/v{version}"
        ));
    }

    // ── Announce dry-render (single-crate) ────────────────────────────────

    /// (1) A broken announce `{{ ReleaseURL }}`-class template fails the guard,
    /// naming the announcer. The exact ReleaseURL-bug regression.
    #[test]
    fn broken_announce_template_aborts_naming_announcer() {
        let config = Config {
            project_name: "widget".to_string(),
            announce: Some(AnnounceConfig {
                discord: Some(DiscordAnnounce {
                    enabled: enabled(),
                    message_template: Some("{{ NoSuchVar }}".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.set_release_url("https://github.com/acme/widget/releases/tag/v1.0.0");

        let err = PrePublishGuardStage
            .run(&mut ctx)
            .expect_err("broken announce template must abort the release");
        let msg = format!("{err:#}");
        assert!(msg.contains("discord"), "names the announcer: {msg}");
    }

    // ── Snapshot / nightly are no-ops ─────────────────────────────────────

    /// (7) Snapshot → guard is a no-op even with a broken template.
    #[test]
    fn snapshot_is_a_noop() {
        let config = Config {
            project_name: "widget".to_string(),
            announce: Some(AnnounceConfig {
                discord: Some(DiscordAnnounce {
                    enabled: enabled(),
                    message_template: Some("{{ NoSuchVar }}".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = ContextOptions {
            snapshot: true,
            ..ContextOptions::default()
        };
        let mut ctx = Context::new(config, opts);
        PrePublishGuardStage
            .run(&mut ctx)
            .expect("snapshot must be a no-op");
    }

    /// (7) Nightly → guard is a no-op even with a broken template.
    #[test]
    fn nightly_is_a_noop() {
        let config = Config {
            project_name: "widget".to_string(),
            announce: Some(AnnounceConfig {
                discord: Some(DiscordAnnounce {
                    enabled: enabled(),
                    message_template: Some("{{ NoSuchVar }}".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = ContextOptions {
            nightly: true,
            ..ContextOptions::default()
        };
        let mut ctx = Context::new(config, opts);
        PrePublishGuardStage
            .run(&mut ctx)
            .expect("nightly must be a no-op");
    }

    /// (7) Regression for the v0.6.0 determinism failure: the harness rebuilds
    /// with `--skip=release,publish,announce` and (on a tag-push run) NOT
    /// `--snapshot`, so neither the snapshot nor nightly skip fires. A broken
    /// announce template whose announce stage is skipped must NOT abort — the
    /// release stage that would populate the release URL is skipped too, so the
    /// template is unrenderable yet nothing will dispatch it.
    #[test]
    fn skipped_stages_are_not_guarded_without_snapshot() {
        let config = Config {
            project_name: "widget".to_string(),
            announce: Some(AnnounceConfig {
                discord: Some(DiscordAnnounce {
                    enabled: enabled(),
                    // References a release-phase var the skipped release stage
                    // never sets — exactly the harness failure mode.
                    message_template: Some("{{ ReleaseURL }}".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = ContextOptions {
            skip_stages: vec![
                "release".to_string(),
                "publish".to_string(),
                "announce".to_string(),
            ],
            ..ContextOptions::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        // Deliberately do NOT set the release URL: the release stage is skipped.
        PrePublishGuardStage
            .run(&mut ctx)
            .expect("a skipped announce stage must not be guarded");
    }

    // ── Publisher manifest render (all three config modes) ────────────────

    fn scoop_cfg(name: &str) -> ScoopConfig {
        ScoopConfig {
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("scoop-bucket".to_string()),
                ..Default::default()
            }),
            name: Some(name.to_string()),
            license: Some("MIT".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            ..Default::default()
        }
    }

    fn crate_with(name: &str, tag_template: &str, publish: PublishConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(publish),
            ..Default::default()
        }
    }

    fn add_windows_zip(ctx: &mut Context, crate_name: &str, binary: &str) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        let target = "x86_64-pc-windows-msvc";
        let mut archive_meta = HashMap::new();
        archive_meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.zip"
            ),
        );
        archive_meta.insert("sha256".to_string(), "a".repeat(64));
        archive_meta.insert("format".to_string(), "zip".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.zip")),
            name: format!("{crate_name}-{target}.zip"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: archive_meta,
            size: None,
        });
        let mut bin_meta = HashMap::new();
        bin_meta.insert("binary".to_string(), binary.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: std::path::PathBuf::from(format!("/dist/{binary}.exe")),
            name: format!("{binary}.exe"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: bin_meta,
            size: None,
        });
    }

    /// (3) A broken publisher manifest template (scoop) aborts the guard
    /// (single-crate). The render fn fails on the undefined var.
    #[test]
    fn broken_scoop_manifest_template_aborts() {
        let mut cfg = scoop_cfg("widget");
        // `homepage` is template-rendered into the manifest with `?`
        // propagation; an undefined var makes the render fn return Err, which
        // the publisher schema validator surfaces.
        cfg.homepage = Some("https://acme.example/{{ NoSuchVar }}".to_string());
        let krate = crate_with(
            "widget",
            "v{{ .Version }}",
            PublishConfig {
                scoop: Some(cfg),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .crates(vec![krate])
            .build();
        scope(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let err = run_guard(&mut ctx, &test_version_resolver())
            .expect_err("broken scoop manifest template must abort");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scoop") || msg.contains("NoSuchVar"),
            "names the publisher / failing var: {msg}"
        );
    }

    /// (3b) A broken scoop `description` aborts the guard. `description` is a
    /// previously-swallowed field (rendered with `unwrap_or_else(|_| raw)`),
    /// now routed through the strict-aware `render_or_warn`; under the guard's
    /// transient render-strict it propagates instead of shipping the raw
    /// `{{ … }}` to the bucket. This is the regression the swallow-hole fix
    /// closes — distinct from the `homepage` case, which already propagated.
    #[test]
    fn broken_scoop_description_aborts_under_guard_strict() {
        let mut cfg = scoop_cfg("widget");
        cfg.description = Some("widget — {{ NoSuchVar }}".to_string());
        let krate = crate_with(
            "widget",
            "v{{ .Version }}",
            PublishConfig {
                scoop: Some(cfg),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .crates(vec![krate])
            .build();
        scope(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let err = run_guard(&mut ctx, &test_version_resolver())
            .expect_err("broken scoop description must abort before publish");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scoop") || msg.contains("NoSuchVar") || msg.contains("description"),
            "names the publisher / failing field: {msg}"
        );
    }

    /// (6) Happy path, single-crate: every template valid → guard Ok.
    #[test]
    fn happy_path_single_crate() {
        let krate = crate_with(
            "widget",
            "v{{ .Version }}",
            PublishConfig {
                scoop: Some(scoop_cfg("widget")),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .crates(vec![krate])
            .build();
        scope(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        run_guard(&mut ctx, &test_version_resolver()).expect("valid single-crate templates pass");
    }

    /// (4) Workspace-lockstep: two crates share one version; one crate's winget
    /// template broken → guard fails naming the crate.
    #[test]
    fn workspace_lockstep_broken_crate_fails() {
        let alpha = crate_with(
            "alpha",
            "v{{ .Version }}",
            PublishConfig {
                scoop: Some(scoop_cfg("alpha")),
                ..Default::default()
            },
        );
        let mut broken = scoop_cfg("beta");
        broken.description = Some("widget — {{ NoSuchVar }}".to_string());
        let beta = crate_with(
            "beta",
            "v{{ .Version }}",
            PublishConfig {
                scoop: Some(broken),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .crates(vec![alpha, beta])
            .build();
        scope(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "alpha", "alpha");
        add_windows_zip(&mut ctx, "beta", "beta");

        let err = run_guard(&mut ctx, &test_version_resolver())
            .expect_err("a broken scoop template in lockstep must abort");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scoop") || msg.contains("beta") || msg.contains("NoSuchVar"),
            "names the crate / publisher / failing var: {msg}"
        );
    }

    /// (5) Workspace per-crate (independent versions): a broken template in the
    /// SECOND crate (version differs from the first) still fails — guards
    /// against an unscoped-render false-negative.
    #[test]
    fn workspace_per_crate_broken_second_crate_fails() {
        let alpha = crate_with(
            "alpha",
            "alpha-v{{ .Version }}",
            PublishConfig {
                scoop: Some(scoop_cfg("alpha")),
                ..Default::default()
            },
        );
        let mut broken = scoop_cfg("beta");
        broken.description = Some("widget — {{ NoSuchVar }}".to_string());
        let beta = crate_with(
            "beta",
            "beta-v{{ .Version }}",
            PublishConfig {
                scoop: Some(broken),
                ..Default::default()
            },
        );
        // Select only the second crate — its own version stamps its manifest.
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope(&mut ctx, "3.1.0");
        add_windows_zip(&mut ctx, "beta", "beta");

        let err = run_guard(&mut ctx, &test_version_resolver())
            .expect_err("a broken template in the second per-crate crate must abort");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("scoop") || msg.contains("beta") || msg.contains("NoSuchVar"),
            "names the crate / publisher / failing var: {msg}"
        );
    }
}
