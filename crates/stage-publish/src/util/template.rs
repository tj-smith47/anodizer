//! Template-rendering helpers — `render_url_template` for `url_template`
//! strings (winget/scoop/krew) and `render_or_warn` for non-strict template
//! evaluation with a logged warning on failure.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars, assert_no_unrendered_logged};
use anyhow::{Result, bail};

/// The build target one `url_template` render is for.
///
/// `os` and `arch` are the publisher's own tokens for the artifact (a
/// publisher may rename them: AUR passes pacman's `x86_64`). `triple` is the
/// artifact's real target triple and `amd64_variant` its `amd64_variant`
/// metadata; together they seed `Target`, `Abi` and the variant variables,
/// so `{{ targetVariant . }}` renders the artifact's own `v3_gnu`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UrlTarget<'a> {
    pub os: &'a str,
    pub arch: &'a str,
    pub triple: &'a str,
    pub amd64_variant: Option<&'a str>,
}

impl<'a> UrlTarget<'a> {
    /// The target of `artifact`, under the publisher's `os` / `arch` tokens.
    pub(crate) fn of(
        artifact: &'a anodizer_core::artifact::Artifact,
        os: &'a str,
        arch: &'a str,
    ) -> Self {
        Self {
            os,
            arch,
            triple: artifact.target.as_deref().unwrap_or(""),
            amd64_variant: artifact.metadata.get("amd64_variant").map(String::as_str),
        }
    }

    /// The target of an [`OsArtifact`](super::OsArtifact), under the
    /// publisher's `os` / `arch` tokens.
    pub(crate) fn of_os_artifact(
        artifact: &'a super::OsArtifact,
        os: &'a str,
        arch: &'a str,
    ) -> Self {
        Self {
            os,
            arch,
            triple: &artifact.target,
            amd64_variant: artifact.amd64_variant.as_deref(),
        }
    }

    /// Write the per-artifact variables onto `vars`: the lower-case
    /// shorthand (`arch`, `os`), `Os`, `Arch`, `Target` (`Abi` follows it)
    /// and the variant variables.
    fn seed(self, vars: &mut TemplateVars) {
        vars.set("arch", self.arch);
        vars.set("os", self.os);
        vars.set("Os", self.os);
        vars.set("Arch", self.arch);
        vars.set("Target", self.triple);
        anodizer_core::archive_name::seed_variant_vars(vars, self.triple, self.amd64_variant);
        // A publisher handing over the bare `arm` token (krew, whose selector
        // is `runtime.GOARCH`) has stripped the version the archive policy
        // keeps in `Arm`; restoring it from the triple lets
        // `{{ Arch }}v{{ Arm }}` render `armv7` here as it does in an archive
        // name. A composite `armv7` token keeps `Arm` empty, as everywhere.
        if self.arch == "arm"
            && let Some(version) = anodizer_core::target::map_target(self.triple)
                .1
                .strip_prefix("armv")
        {
            vars.set("Arm", version);
        }
    }
}

/// Render a `url_template` string with Tera, providing only the per-artifact
/// variables: `name`, `version` and everything [`UrlTarget`] seeds.
///
/// Prefer [`render_url_template_with_ctx`] for new call sites — that variant
/// also exposes the full project template surface (`ProjectName`, `Tag`,
/// `Version`, `Env.*`, `ArtifactName`, etc.) so a dotted-variable config
/// like `url_template: "{{ .Tag }}/{{ .ArtifactName }}"` resolves correctly.
/// This thin wrapper is retained for the rare site that has no `&Context`
/// available.
pub(crate) fn render_url_template(
    url_template: &str,
    name: &str,
    version: &str,
    target: UrlTarget<'_>,
) -> String {
    let mut vars = TemplateVars::new();
    vars.set("name", name);
    vars.set("version", version);
    target.seed(&mut vars);
    template::render(url_template, &vars).unwrap_or_else(|_| url_template.to_string())
}

/// Render a `url_template` string with the full context template-vars surface
/// (Tag, ProjectName, Version, Env.\*, Major/Minor/Patch, Commit, Branch,
/// PreviousTag, ArtifactName, …) plus the per-artifact overlays (`name`,
/// `version`, `ArtifactName` and everything [`UrlTarget`] seeds).
///
/// On render error (malformed template), returns the raw input unchanged —
/// matching the [`render_url_template`] failure path.
pub(crate) fn render_url_template_with_ctx(
    ctx: &Context,
    url_template: &str,
    name: &str,
    version: &str,
    target: UrlTarget<'_>,
) -> String {
    render_url_template_with_ctx_and_artifact(ctx, url_template, name, None, version, target)
}

/// Like [`render_url_template_with_ctx`] but also sets `ArtifactName`
/// unconditionally from an explicit artifact filename.
///
/// Use this variant when the caller has a project/crate `name` (no extension)
/// AND a separate `artifact_name` (the archive filename, e.g.
/// `tool-1.2.0-linux-amd64.tar.gz`). The `name` project token and the
/// `ArtifactName` archive filename are then independently available in the
/// template.
pub(crate) fn render_url_template_with_ctx_and_artifact(
    ctx: &Context,
    url_template: &str,
    name: &str,
    artifact_name: Option<&str>,
    version: &str,
    target: UrlTarget<'_>,
) -> String {
    let mut vars = ctx.template_vars().clone();
    vars.set("name", name);
    vars.set("version", version);
    target.seed(&mut vars);
    // An explicit artifact filename takes precedence. Without one,
    // `ArtifactName` is set only when `name` itself looks like a filename
    // (has an extension), which keeps a project or cask token out of it.
    if let Some(filename) = artifact_name.or_else(|| name.contains('.').then_some(name)) {
        vars.set("ArtifactName", filename);
    }
    template::render(url_template, &vars).unwrap_or_else(|_| url_template.to_string())
}

/// Render `raw` through the context template engine, named by `field` (e.g.
/// `"aur.name"`, `"winget.description"`, `"scoop.name"`).
///
/// Behaviour depends on [`Context::render_is_strict`]:
/// - **Strict** (the pre-publish guard's render pass, or the user's global
///   `--strict`): a malformed template returns `Err`, naming the `field`, so a
///   broken publisher template fails the release loud BEFORE any irreversible
///   publisher fires.
/// - **Lenient** (production dry-run / snapshot / nightly publish): a malformed
///   template logs a `log.warn` describing the failed `field` and falls back to
///   the raw value, keeping a currently-malformed config building while making
///   the error visible in stage output.
///
/// Earlier these sites used `ctx.render_template(...).unwrap_or_else(|_|
/// raw.clone())`, which silently swallowed malformed-template errors and
/// propagated the raw string downstream — defeating both debuggability and the
/// guard. Routing every site through here closes that gap.
///
/// `field` should carry the namespace (e.g. `"aur.name"`,
/// `"aur_source.directory"`); the warn message does not prepend a stage
/// prefix because `StageLogger` already does that for every line.
pub(crate) fn render_or_warn(
    ctx: &Context,
    log: &StageLogger,
    field: &str,
    raw: &str,
) -> Result<String> {
    render_or_warn_with_vars(ctx.template_vars(), log, field, raw, ctx.render_is_strict())
}

/// Like [`render_or_warn`], but renders against an explicit `vars` set instead
/// of the context's global template vars, and takes `is_strict` directly
/// (callers pass [`Context::render_is_strict`]) since there is no `&Context`
/// in hand.
///
/// Used where a publisher scopes an extra template variable for a single
/// resource's renders (e.g. the AUR-source `Amd64` micro-architecture variable)
/// and the `directory:` / `url_template:` strings must see that same scoped
/// value — rendering them against the global vars would resolve the scoped
/// variable to its stale/empty global value.
pub(crate) fn render_or_warn_with_vars(
    vars: &TemplateVars,
    log: &StageLogger,
    field: &str,
    raw: &str,
    is_strict: bool,
) -> Result<String> {
    match template::render(raw, vars) {
        Ok(rendered) => Ok(rendered),
        Err(e) => {
            if is_strict {
                bail!("failed to render {field} template {raw:?}: {e}");
            }
            log.warn(&format!(
                "failed to render {field} template {raw:?}: {e}; \
                 falling back to raw value"
            ));
            Ok(raw.to_string())
        }
    }
}

/// Final-text guard: after a publisher renders its manifest to the finished
/// `text`, assert it carries no residual Go/Tera `{{ … }}` delimiters — a
/// residual means a user-supplied config string field was emitted without
/// being template-rendered (the bug class this guard makes unrepresentable).
///
/// `label` names the publisher + manifest (e.g. `"chocolatey nuspec"`). The
/// offending snippet is redacted via [`Context::redact`] before it reaches any
/// log line or error, so a secret-flagged value never leaks.
///
/// - **strict**: returns `Err`, failing the publish before any irreversible
///   step. Strict whenever [`Context::render_is_strict`] is set (the
///   prepublish guard's render pass, or the user's global `--strict`) OR
///   whenever this is a real publish — not [`Context::is_dry_run`], not
///   [`Context::is_snapshot`] — since a residual delimiter reaching an actual
///   publisher call renders an invalid field that the registry may reject
///   after submission, with no way to undo it.
/// - **non-strict** (dry-run / snapshot, and not otherwise strict):
///   `log.warn`s the redacted snippet and returns `Ok(())`.
///
/// Intended for manifest formats that never legitimately contain `{{ }}`
/// (nuspec/JSON/YAML/Ruby/nix/PKGBUILD); do NOT apply to verbatim/raw paths
/// (e.g. announce `--raw`) where unrendered delimiters are by design. Apply
/// this at every irreversible manifest-writing publisher's final assembled
/// manifest text, immediately before that publisher's first side-effecting
/// step (clone/commit/push/upload) — never to a raw/verbatim passthrough
/// field.
pub(crate) fn guard_no_unrendered(
    ctx: &Context,
    log: &StageLogger,
    label: &str,
    text: &str,
) -> Result<()> {
    // A manifest bound for an irreversible publisher must never contain a
    // residual `{{ }}` — it renders an invalid URL/field that a registry
    // (e.g. Chocolatey's blocking PackageSourceUrlValidRequirement) rejects
    // after submission, when nothing can be undone. Dry-run and snapshot
    // stay lenient: neither publishes anything irreversible, and the
    // determinism harness / nightly builds can legitimately hit an
    // unrenderable field.
    let strict = ctx.render_is_strict() || (!ctx.is_dry_run() && !ctx.is_snapshot());
    assert_no_unrendered_logged(text, label, strict, |s| ctx.redact(s), |msg| log.warn(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::Verbosity;

    const RESIDUAL: &str = "https://example.com/tree/{{ .Tag }}/crates/cli";

    fn ctx_with_opts(opts: ContextOptions) -> Context {
        Context::new(Config::default(), opts)
    }

    fn log() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    #[test]
    fn real_publish_hard_fails_on_residual_delimiters() {
        let ctx = ctx_with_opts(ContextOptions {
            dry_run: false,
            snapshot: false,
            strict: false,
            ..ContextOptions::default()
        });
        let err = guard_no_unrendered(&ctx, &log(), "chocolatey nuspec", RESIDUAL)
            .expect_err("residual {{ }} must hard-fail before an irreversible publish");
        assert!(
            err.to_string().contains("chocolatey nuspec"),
            "error should name the manifest label: {err}"
        );
    }

    #[test]
    fn dry_run_stays_lenient_on_residual_delimiters() {
        let ctx = ctx_with_opts(ContextOptions {
            dry_run: true,
            snapshot: false,
            strict: false,
            ..ContextOptions::default()
        });
        guard_no_unrendered(&ctx, &log(), "chocolatey nuspec", RESIDUAL)
            .expect("dry-run must warn-and-continue, not fail");
    }

    #[test]
    fn snapshot_stays_lenient_on_residual_delimiters() {
        let ctx = ctx_with_opts(ContextOptions {
            dry_run: false,
            snapshot: true,
            strict: false,
            ..ContextOptions::default()
        });
        guard_no_unrendered(&ctx, &log(), "chocolatey nuspec", RESIDUAL)
            .expect("snapshot must warn-and-continue, not fail");
    }

    #[test]
    fn real_publish_still_passes_a_fully_rendered_manifest() {
        let ctx = ctx_with_opts(ContextOptions {
            dry_run: false,
            snapshot: false,
            strict: false,
            ..ContextOptions::default()
        });
        guard_no_unrendered(
            &ctx,
            &log(),
            "chocolatey nuspec",
            "https://example.com/tree/v1.2.3/crates/cli",
        )
        .expect("a fully-rendered manifest must never fail the guard");
    }
}
