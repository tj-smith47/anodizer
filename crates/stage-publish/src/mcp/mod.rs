//! MCP (Model Context Protocol) server registry publisher.
//!
//! Posts a server JSON document to `{registry}/v0/publish` after
//! exchanging the configured credentials for a registry JWT. Two design
//! choices worth calling out:
//!
//! 1. **No on-disk token files.** Tokens are kept in memory rather than
//!    written to the cwd and deleted post-publish — same wire behaviour,
//!    fewer artefacts left behind on disk if the process dies mid-publish.
//! 2. **No separate defaulting phase.** There is no nested `mcp.github:`
//!    block to migrate, and `McpAuth::default()` already defaults to
//!    `None`, so no `Default` phase is needed here.
//!
//! Skip gate:
//!   - `ctx.should_skip("mcp")` (uniform `--skip=mcp` flag), OR
//!   - `ctx.config.mcp.name` is unset/empty, OR
//!   - `ctx.config.mcp.skip` evaluates truthy.

mod auth;
mod manifest;
pub mod publisher;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result};

use anodizer_core::config::{McpAuthMethod, McpConfig};

use auth::{build_client, provider_for};
use manifest::{
    CURRENT_SCHEMA_URL, DEFAULT_REGISTRY_URL, Header, Package, Repository, ServerJson,
    ServerResponse, Transport,
};

/// Serialized shape of a recorded MCP publish. Single-entry per run —
/// MCP is top-level (one `mcp:` block) — so we still store it as a Vec
/// for shape-parity with the krew/homebrew/scoop targets.
///
/// `server_name` is the rendered `mcp.name` (already template-resolved)
/// and `registry_url` is the resolved endpoint (config override or
/// [`DEFAULT_REGISTRY_URL`]).
///
/// `version` and `auth_method` are captured at publish time so rollback
/// can reconstruct the PATCH URL and re-authenticate without reading config
/// (which might have changed between publish and rollback invocations).
///
/// Constructed only on the success path of [`publish_with_registry`] —
/// dry-run, skip-true, and missing-name short-circuits return `None` so
/// no phantom evidence ever lands in [`anodizer_core::PublishEvidence::extra`].
///
/// Aliased to the core-owned snapshot so the evidence schema lives
/// in [`anodizer_core::publish_evidence`] and credential-shaped
/// fields (`token`, `password`, `pat`) have no slot to land in. See
/// [`publisher`] module rustdoc for the credential-handling
/// rationale.
pub(crate) type McpTarget = anodizer_core::publish_evidence::McpTargetSnapshot;

/// Process-wide flag — the "mcp is experimental" warning is emitted at
/// most once per anodizer invocation regardless of how many crates trigger
/// the publisher.
static EXPERIMENTAL_WARNED: AtomicBool = AtomicBool::new(false);

/// Top-level entry point — dispatched from `PublishStage::run` under the
/// shared `top_level!` macro. Performs the skip-gate, builds the manifest
/// from `ctx.config.mcp`, exchanges credentials for a registry JWT, and
/// posts the manifest with retries.
///
/// Returns `Some(McpTarget)` describing what was published when the POST
/// succeeds; `None` when the skip-gate fires (missing name, truthy
/// `mcp.skip`, or `--dry-run`). The wrapper's `Publisher::run` records the
/// returned target in `evidence.extra` only when `Some`, so a later
/// `--rollback-only` cannot fire a PATCH against a server-version that
/// was never published.
pub(crate) fn publish_to_mcp(ctx: &mut Context, log: &StageLogger) -> Result<Option<McpTarget>> {
    // In per-crate iteration (workspace publish-only), `selected_crates`
    // is scoped to a single crate per pass. The mcp block is top-level
    // (one `mcp:` per config), so without this gate the publisher would
    // fire on EVERY crate's pass — including crates that don't own the OCI
    // image the manifest references, whose image may not even be built yet.
    // Run mcp only on the pass for the crate that owns the referenced image.
    if !mcp_image_owned_by_selected(ctx) {
        // verbose (not status): this fires on N-1 of N crate passes in a
        // workspace, so promoting it to status would flood normal output.
        log.verbose(
            "mcp: skipping — none of the selected crates own the OCI image \
             referenced by the mcp manifest (runs on the owning crate's pass)",
        );
        ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
            anodizer_core::SkipReason::NotApplicable,
        ));
        return Ok(None);
    }
    let registry_url = resolve_registry_url(&ctx.config.mcp).to_string();
    publish_with_registry(ctx, log, &registry_url)
}

/// Decide whether the mcp publisher should run for the current crate
/// selection.
///
/// - When `ctx.options.selected_crates` is empty (non-workspace / single
///   config, or an explicit run-once context), returns `true` — the
///   publisher keeps its run-once behavior.
/// - When `selected_crates` is non-empty (per-crate iteration), returns
///   `true` only if one of the selected crates owns the OCI image the mcp
///   manifest references. Ownership is decided by comparing the image
///   *repo* (the ref minus its `:tag`) so that an un-rendered
///   `{{ .Version }}` template in either the mcp identifier or the crate's
///   `docker_v2[].images` never affects the match.
///
/// When the mcp manifest references no OCI package, every crate's pass is
/// allowed through — the publisher's own skip-gate then decides whether it
/// has anything to do.
fn mcp_image_owned_by_selected(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    if selected.is_empty() {
        return true;
    }

    let mcp_repos: Vec<&str> = ctx
        .config
        .mcp
        .packages
        .iter()
        .filter(|p| p.registry_type == anodizer_core::config::McpRegistryType::Oci)
        .map(|p| image_repo(&p.identifier))
        .collect();
    if mcp_repos.is_empty() {
        return true;
    }

    crate::util::all_crates(ctx)
        .iter()
        .filter(|c| selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.docker_v2.as_ref())
        .flatten()
        .flat_map(|d| d.images.iter())
        .any(|img| mcp_repos.contains(&image_repo(img)))
}

/// Return the repository portion of a container image reference — the ref
/// minus any `:tag` suffix. A trailing tag is only stripped when the last
/// path segment (after the final `/`) contains a `:`, so a registry host
/// port such as `registry:5000/owner/app` is preserved while
/// `ghcr.io/owner/app:v1.2.3` becomes `ghcr.io/owner/app`. A `@sha256:`
/// digest reference is likewise trimmed to the repo.
fn image_repo(image: &str) -> &str {
    let image = image.split_once('@').map_or(image, |(repo, _)| repo);
    let last_segment_start = image.rfind('/').map_or(0, |i| i + 1);
    match image[last_segment_start..].find(':') {
        Some(rel) => &image[..last_segment_start + rel],
        None => image,
    }
}

/// Resolve the effective registry URL with the standard fallback chain:
/// trim the configured value, treat empty/whitespace as unset, and fall
/// back to [`DEFAULT_REGISTRY_URL`]. Pure function over the config so
/// the fallback is independently testable without spinning up a context
/// or the publish loop.
pub(crate) fn resolve_registry_url(mcp: &McpConfig) -> &str {
    mcp.registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGISTRY_URL)
}

/// Test-friendly variant — accepts a registry base URL override. Production
/// callers go through `publish_to_mcp`, which honours `ctx.config.mcp.registry`
/// (falling back to `manifest::DEFAULT_REGISTRY_URL`). Tests inject the
/// wiremock-style `http://127.0.0.1:<port>` address of a one-shot HTTP
/// responder directly.
///
/// Returns `Some(McpTarget)` only after the `/v0/publish` POST succeeds.
/// All short-circuit paths (skip-true, missing name, dry-run) return `None`.
pub(crate) fn publish_with_registry(
    ctx: &mut Context,
    log: &StageLogger,
    registry_url: &str,
) -> Result<Option<McpTarget>> {
    // ---- Skip gate + render (single evaluation) ----
    // Evaluate the name / `skip` / `if` gate exactly once via the shared
    // pipeline the schema validator also drives, then record the per-reason
    // `Skipped` outcome + log line from that single verdict. Gating here and
    // re-gating in the render path would render the `skip`/`if` templates twice
    // and let the predicate drift between two homes.
    let mcp_rendered = match render_mcp_config(ctx)? {
        McpRenderOutcome::Rendered(cfg) => *cfg,
        McpRenderOutcome::Skipped(skip) => {
            log.status(skip.message);
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(skip.reason));
            return Ok(None);
        }
    };

    // ---- One-shot experimental warning ----
    warn_experimental_once(log);

    // ---- Assemble the ServerJSON payload ----
    // Built here (before the dry-run short-circuit) so the resolved
    // `server.name` drives the dry-run log line, the publish POST, and the
    // recorded target from one value — no separate defensive name binding.
    let server = build_server_json(&mcp_rendered, &ctx.version());

    // ---- Dry-run short-circuits before any network I/O ----
    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish to MCP registry {} as '{}' (auth={})",
            registry_url,
            server.name,
            mcp_rendered.auth.method.as_str()
        ));
        ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
            anodizer_core::SkipReason::DryRun,
        ));
        return Ok(None);
    }

    let policy = ctx.retry_policy();

    // Surface the env-var fallback path BEFORE constructing the provider.
    // When `auth.type=github` and `auth.token` rendered empty (e.g. the user
    // templated `{{ .Env.GITHUB_TOKEN }}` and GITHUB_TOKEN is unset),
    // `GithubAtAuthProvider::get_token` silently falls back to
    // `MCP_GITHUB_TOKEN`. Log the resolution path (NOT the token value) so a
    // user debugging an auth failure can see what anodizer tried.
    if mcp_rendered.auth.method == McpAuthMethod::Github && mcp_rendered.auth.token.is_empty() {
        log.status("mcp: auth.token empty, falling back to MCP_GITHUB_TOKEN env var");
    }

    // ---- Build + authenticate ----
    let provider = provider_for(
        mcp_rendered.auth.method,
        registry_url,
        &mcp_rendered.auth.token,
        &policy,
    );
    provider.login().context("mcp: could not login")?;
    let token = provider
        .get_token()
        .context("mcp: could not get registry token")?;

    let body = serde_json::to_string(&server).context("mcp: serialize ServerJSON")?;

    // ---- POST /v0/publish with retries ----
    let publish_url = format!("{}/v0/publish", registry_url.trim_end_matches('/'));
    publish_payload(
        &publish_url,
        &body,
        &token,
        &policy,
        log,
        &server.name,
        registry_url,
    )?;

    // Only construct the target on the success path so rollback evidence
    // tracks exactly what landed on the registry. `server.name` is the
    // rendered name; `registry_url` is the resolved endpoint.
    Ok(Some(McpTarget {
        target: server.name.clone(),
        server_name: server.name,
        registry_url: registry_url.to_string(),
        version: ctx.version(),
        auth_method: mcp_rendered.auth.method,
    }))
}

/// Why the MCP publisher produced no document for this run — the single skip
/// gate's verdict, carrying both the outcome reason the live publish records
/// and the user-facing status line it logs.
///
/// One value per skip branch keeps the gate predicate and its per-reason
/// reporting in lockstep: [`render_mcp_config`] decides, and
/// [`publish_with_registry`] derives its `Skipped` outcome + log line from the
/// same verdict rather than re-evaluating the gates.
struct McpSkip {
    /// Outcome reason recorded into the publisher outcome map.
    reason: anodizer_core::SkipReason,
    /// Status line shown to the user on the live publish path.
    message: &'static str,
}

/// The verdict of the MCP render pipeline: either the fully-templated config to
/// publish, or the skip verdict explaining why there is nothing to publish.
enum McpRenderOutcome {
    /// The publisher is configured and enabled; carries the rendered config.
    Rendered(Box<McpConfig>),
    /// The publisher is unconfigured / disabled; carries the skip verdict.
    Skipped(McpSkip),
}

/// Evaluate the MCP skip gate once and, when it passes, render the
/// fully-templated [`McpConfig`] this run would publish.
///
/// Drives the same side-effect-free pipeline the live publish runs — evaluate
/// the name / `skip` / `if` gate, then (on pass) clone the top-level `mcp`
/// block, apply the project-metadata fallback, template every string field, and
/// infer the repository — without recording outcomes, emitting the experimental
/// banner, or touching the network. The fill / render / infer helpers mutate
/// only the local clone (they take `ctx` immutably and read config / git
/// metadata), so this is safe to call from the schema validator on a shared
/// `&Context`.
///
/// Returns [`McpRenderOutcome::Skipped`] (with the reason + log line) for an
/// unset/empty `mcp.name`, a truthy `mcp.skip`, or a falsy `mcp.if`, so the live
/// publish derives its per-reason outcome from this single evaluation rather
/// than re-gating.
fn render_mcp_config(ctx: &Context) -> Result<McpRenderOutcome> {
    if ctx.config.mcp.name.as_deref().unwrap_or("").is_empty() {
        return Ok(McpRenderOutcome::Skipped(McpSkip {
            reason: anodizer_core::SkipReason::NotConfigured,
            message: "mcp: skipping — no mcp.name configured",
        }));
    }
    let mut mcp_rendered: McpConfig = ctx.config.mcp.clone();
    if let Some(skip) = mcp_rendered.skip.as_ref() {
        let off = skip
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("mcp: render skip template")?;
        if off {
            return Ok(McpRenderOutcome::Skipped(McpSkip {
                reason: anodizer_core::SkipReason::NotApplicable,
                message: "mcp: skipping — skip evaluates true",
            }));
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        mcp_rendered.if_condition.as_deref(),
        "mcp publisher",
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        return Ok(McpRenderOutcome::Skipped(McpSkip {
            reason: anodizer_core::SkipReason::NotApplicable,
            message: "mcp: skipping — `if` condition evaluated falsy",
        }));
    }

    fill_from_project_metadata(ctx, &mut mcp_rendered);
    render_strings(ctx, &mut mcp_rendered)?;
    infer_repository_from_release(ctx, &mut mcp_rendered);
    Ok(McpRenderOutcome::Rendered(Box::new(mcp_rendered)))
}

/// Render the exact [`ServerJson`] document this run would POST to the registry,
/// or `None` when the MCP publisher is not configured / disabled.
///
/// One source of truth for the published payload: both [`publish_with_registry`]
/// and the schema validator obtain the server document from the same
/// [`render_mcp_config`] pipeline plus [`build_server_json`], so what gets
/// validated is byte-for-byte what ships. Version is the global
/// [`Context::version`] — MCP publishes a single top-level `server.json` per
/// release, not a per-crate document.
pub(crate) fn render_server_json(ctx: &Context) -> Result<Option<ServerJson>> {
    let McpRenderOutcome::Rendered(mcp_rendered) = render_mcp_config(ctx)? else {
        return Ok(None);
    };
    Ok(Some(build_server_json(&mcp_rendered, &ctx.version())))
}

/// Populate `mcp.description` and `mcp.homepage` from the project's
/// top-level `metadata:` block when the MCP config leaves them empty.
/// Mirrors the fallback pattern used by Homebrew (formula + cask),
/// Snapcraft, and DockerHub.
fn fill_from_project_metadata(ctx: &Context, mcp: &mut McpConfig) {
    // The mcp block is top-level but runs on its owning crate's pass (the
    // crate whose `docker_v2` image the manifest references); derive metadata
    // from THAT crate's `Cargo.toml`, falling back to the project primary
    // crate when no OCI package pins ownership.
    let owner = mcp_owning_crate_name(ctx);
    let description = owner
        .and_then(|c| ctx.config.meta_description_for(c))
        .or_else(|| ctx.config.meta_description_project());
    let homepage = owner
        .and_then(|c| ctx.config.meta_homepage_for(c))
        .or_else(|| ctx.config.meta_homepage_project());
    if mcp.description.as_deref().is_none_or(str::is_empty)
        && let Some(d) = description
    {
        mcp.description = Some(d.to_string());
    }
    if mcp.homepage.as_deref().is_none_or(str::is_empty)
        && let Some(h) = homepage
    {
        mcp.homepage = Some(h.to_string());
    }
}

/// Name of the crate that owns the OCI image referenced by the mcp manifest,
/// for per-crate metadata derivation. `None` when the manifest references no
/// OCI package (no ownership to pin) — callers then fall back to the project
/// primary crate.
fn mcp_owning_crate_name(ctx: &Context) -> Option<&str> {
    let mcp_repos: Vec<&str> = ctx
        .config
        .mcp
        .packages
        .iter()
        .filter(|p| p.registry_type == anodizer_core::config::McpRegistryType::Oci)
        .map(|p| image_repo(&p.identifier))
        .collect();
    if mcp_repos.is_empty() {
        return None;
    }
    ctx.config
        .crates
        .iter()
        .chain(
            ctx.config
                .workspaces
                .iter()
                .flatten()
                .flat_map(|w| w.crates.iter()),
        )
        .find(|c| {
            c.docker_v2
                .as_ref()
                .into_iter()
                .flatten()
                .flat_map(|d| d.images.iter())
                .any(|img| mcp_repos.contains(&image_repo(img)))
        })
        .map(|c| c.name.as_str())
}

/// Fill in `mcp.repository.url` / `mcp.repository.source` when the user left
/// them blank, deriving from (in priority order):
///
/// 1. the configured `release.<github|gitlab|gitea>` block, or
/// 2. the `origin` git remote when it is a GitHub repo (source `github`).
///
/// Without this, the entire `repository` object is silently omitted from the
/// published payload (the builder gates on `mcp.repository.url.is_empty()`).
/// A user-set `repository.url` always wins — derivation only fills a genuine
/// gap, and a user-set `repository.source` is likewise preserved.
///
/// The git-remote fallback is GitHub-only by construction: a self-hosted or
/// non-GitHub `origin` never matches [`parse_github_remote`], so the
/// `repository` object stays user-supplied rather than being force-derived
/// with a wrong `source`.
fn infer_repository_from_release(ctx: &Context, mcp: &mut McpConfig) {
    if !mcp.repository.url.is_empty() {
        return;
    }
    let resolved = resolve_repository_host(&ctx.config).or_else(github_repo_from_remote);
    apply_inferred_repository(mcp, resolved);
}

/// Resolve `(host, owner, name)` from the configured `release.<host>` block.
/// Returns `None` when there is no release block or none of the SCM sub-blocks
/// carry a non-empty owner+name.
fn resolve_repository_host(
    config: &anodizer_core::config::Config,
) -> Option<(&'static str, String, String)> {
    let release = config.release.as_ref()?;
    let (host, repo) = if let Some(r) = release.github.as_ref() {
        ("github", r)
    } else if let Some(r) = release.gitlab.as_ref() {
        ("gitlab", r)
    } else if let Some(r) = release.gitea.as_ref() {
        ("gitea", r)
    } else {
        return None;
    };
    if repo.owner.is_empty() || repo.name.is_empty() {
        return None;
    }
    Some((host, repo.owner.clone(), repo.name.clone()))
}

/// Resolve `(host="github", owner, name)` from the `origin` git remote when it
/// is a GitHub repo. Reads `git remote get-url origin` from the process cwd
/// (the project root the publisher runs in), matching `auto_detect_github`'s
/// cwd-based probe. Returns `None` for any non-GitHub remote (so derivation
/// never invents a wrong `source`) and for any git failure (no remote,
/// detached checkout, ...).
fn github_repo_from_remote() -> Option<(&'static str, String, String)> {
    anodizer_core::git::detect_github_repo()
        .ok()
        .map(|(owner, name)| ("github", owner, name))
}

/// Apply a resolved `(host, owner, name)` to `mcp.repository`, leaving any
/// user-set `source` untouched. Pure over its inputs so both derivation
/// branches (release block, git remote) funnel through one tested writer.
fn apply_inferred_repository(
    mcp: &mut McpConfig,
    resolved: Option<(&'static str, String, String)>,
) {
    let Some((host, owner, name)) = resolved else {
        return;
    };
    let base = match host {
        "github" => "https://github.com",
        "gitlab" => "https://gitlab.com",
        "gitea" => "https://gitea.com",
        _ => return,
    };
    mcp.repository.url = format!("{}/{}/{}", base, owner, name);
    if mcp.repository.source.is_empty() {
        mcp.repository.source = host.to_string();
    }
}

/// Emit the experimental-warning banner the first time the publisher runs in
/// this process; subsequent invocations are silent. The atomic flag is
/// process-wide.
///
/// Returns `true` if THIS call emitted the warning (the swap flipped the
/// flag from `false` to `true`), `false` otherwise. The return value lets
/// tests assert one-shot semantics without depending on test-ordering
/// (a previous in-process test could have already flipped the flag, so
/// inspecting the static directly is race-prone). Production callers
/// can ignore the return value.
fn warn_experimental_once(log: &StageLogger) -> bool {
    if !EXPERIMENTAL_WARNED.swap(true, Ordering::SeqCst) {
        log.warn(
            "mcp is experimental and subject to change. Keep an eye on the \
             release notes if you wish to rely on this for production builds; \
             feedback at https://github.com/tj-smith47/anodizer/issues",
        );
        true
    } else {
        false
    }
}

/// Apply `ctx.render_template` to every templatable string in `mcp`.
///
/// Renders the top-level fields (name, description, ...) and the
/// per-package identifier. One-stop helper so the publisher orchestration
/// stays linear.
fn render_strings(ctx: &Context, mcp: &mut McpConfig) -> Result<()> {
    render_opt_in_place(ctx, &mut mcp.name, "name")?;
    render_opt_in_place(ctx, &mut mcp.description, "description")?;
    render_opt_in_place(ctx, &mut mcp.title, "title")?;
    render_opt_in_place(ctx, &mut mcp.homepage, "homepage")?;
    render_in_place(ctx, &mut mcp.repository.url, "repository.url")?;
    render_in_place(ctx, &mut mcp.repository.source, "repository.source")?;
    render_in_place(ctx, &mut mcp.repository.id, "repository.id")?;
    render_in_place(ctx, &mut mcp.repository.subfolder, "repository.subfolder")?;

    // auth.type is rendered by serializing the enum to its wire-format
    // string, rendering that, and re-parsing — so a user can write
    // `auth.type: "{{ if eq .Env.MODE \"ci\" }}github-oidc{{ else }}none{{ end }}"`
    // and have it resolve at publish time.
    let mut type_str = mcp.auth.method.as_str().to_string();
    render_in_place(ctx, &mut type_str, "auth.type")?;
    mcp.auth.method = McpAuthMethod::parse(&type_str).context("mcp: auth.type template result")?;
    render_in_place(ctx, &mut mcp.auth.token, "auth.token")?;

    for (i, pkg) in mcp.packages.iter_mut().enumerate() {
        render_in_place(
            ctx,
            &mut pkg.identifier,
            &format!("packages[{}].identifier", i),
        )?;
        render_in_place(
            ctx,
            &mut pkg.transport.url,
            &format!("packages[{}].transport.url", i),
        )?;
        for (j, header) in pkg.transport.headers.iter_mut().enumerate() {
            render_in_place(
                ctx,
                &mut header.value,
                &format!("packages[{}].transport.headers[{}].value", i, j),
            )?;
        }
    }
    Ok(())
}

/// Render a single string in place, replacing it with the rendered form on
/// success and bubbling a labelled error on failure.
fn render_in_place(ctx: &Context, s: &mut String, label: &str) -> Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    let rendered = ctx
        .render_template(s)
        .with_context(|| format!("mcp: render {} template", label))?;
    *s = rendered;
    Ok(())
}

/// Render an `Option<String>` in place when populated and non-empty.
fn render_opt_in_place(ctx: &Context, s: &mut Option<String>, label: &str) -> Result<()> {
    if let Some(val) = s.as_mut() {
        render_in_place(ctx, val, label)?;
    }
    Ok(())
}

/// Assemble the `ServerJson` payload from a fully-templated `McpConfig`.
///
/// Server-document assembly:
///   - `repository` is omitted entirely when `mcp.Repository.URL == ""`.
///   - Per-package `version` is `ctx.Version` for all registry types
///     **except** `oci`, which forces `""` (the version is embedded in
///     the OCI image identifier's `:tag` suffix).
pub(crate) fn build_server_json(mcp: &McpConfig, version: &str) -> ServerJson {
    let repository = if mcp.repository.url.is_empty() {
        None
    } else {
        Some(Repository {
            url: mcp.repository.url.clone(),
            source: mcp.repository.source.clone(),
            id: mcp.repository.id.clone(),
            subfolder: mcp.repository.subfolder.clone(),
        })
    };

    let packages = mcp
        .packages
        .iter()
        .map(|pkg| {
            // OCI image refs already pin the version (e.g.
            // `ghcr.io/foo/bar:v1.2.3`), so the registry's
            // `body.packages[i].version` field is redundant for OCI
            // packages. Combined with `Package::version`'s
            // `skip_serializing_if = "String::is_empty"`, the empty
            // value is omitted on the wire — matching upstream Go's
            // `json:"version,omitempty"` and satisfying the openapi
            // schema's `minLength: 1` constraint.
            let v = if pkg.registry_type == anodizer_core::config::McpRegistryType::Oci {
                String::new()
            } else {
                version.to_string()
            };
            Package {
                registry_type: pkg.registry_type.as_str().to_string(),
                identifier: pkg.identifier.clone(),
                version: v,
                transport: Transport {
                    kind: pkg.transport.kind.as_str().to_string(),
                    url: pkg.transport.url.clone(),
                    headers: pkg
                        .transport
                        .headers
                        .iter()
                        .map(|h| Header {
                            name: h.name.clone(),
                            value: h.value.clone(),
                        })
                        .collect(),
                },
            }
        })
        .collect();

    ServerJson {
        schema: CURRENT_SCHEMA_URL.to_string(),
        name: mcp.name.clone().unwrap_or_default(),
        description: mcp.description.clone().unwrap_or_default(),
        title: mcp.title.clone().unwrap_or_default(),
        repository,
        version: version.to_string(),
        website_url: mcp.homepage.clone().unwrap_or_default(),
        packages,
    }
}

/// POST `body` to `publish_url` with `Authorization: Bearer <token>`,
/// retrying transient 5xx / 429 / network failures per `policy`. 200 and
/// 201 are both treated as success (the registry uses 201 for fresh
/// publishes, 200 for re-publishes / status updates). 4xx fast-fails.
///
/// On success, parses the response body for `_meta.official.status` and
/// logs `published to MCP registry name=<...> status=<...>`.
/// Maximum response-body bytes embedded in the user-visible HTTP-error
/// message. Cap is byte-based (cheap `len()`) instead of char-based so a
/// large response body is rejected after a single O(1) length check —
/// the prior `chars().take(512)` + `chars().count() > 512` shape walked a
/// 100 KB body twice. The cut walks back to a UTF-8 char boundary so we
/// never slice through a multi-byte char.
const MAX_RESPONSE_SNIPPET_BYTES: usize = 512;

/// Return `(snippet, truncated_suffix)` for a scrubbed HTTP response
/// body. When the body fits inside [`MAX_RESPONSE_SNIPPET_BYTES`] the
/// snippet borrows the input verbatim and the suffix is empty;
/// otherwise the snippet owns a copy truncated at the nearest UTF-8
/// char boundary at or below the byte cap and the suffix is
/// `"...[truncated]"`.
///
/// `Cow` over `String` so the dominant short-body case (most registry
/// responses are well under 512 B) avoids an allocation — the formatter
/// at the call site handles both shapes transparently.
fn truncate_response_snippet(scrubbed: &str) -> (std::borrow::Cow<'_, str>, &'static str) {
    if scrubbed.len() <= MAX_RESPONSE_SNIPPET_BYTES {
        return (std::borrow::Cow::Borrowed(scrubbed), "");
    }
    let mut cut = MAX_RESPONSE_SNIPPET_BYTES;
    // UTF-8 multi-byte chars must not be split; `is_char_boundary(0)` is
    // always true so the loop terminates without an underflow guard.
    while !scrubbed.is_char_boundary(cut) {
        cut -= 1;
    }
    (
        std::borrow::Cow::Owned(scrubbed[..cut].to_string()),
        "...[truncated]",
    )
}

/// Map a registry `/v0/publish` rejection body to an anodizer-aware
/// remediation hint, or `""` when no specific guidance applies.
///
/// The registry's OCI validator
/// fails closed when it cannot prove image ownership: the published image
/// must carry an `io.modelcontextprotocol.server.name` **image config label**
/// equal to the server name. Its own error text says "Add this to your
/// Dockerfile: LABEL …", which under-serves anodizer users who build images
/// through the `docker_v2:` block and have no hand-written `LABEL` line — and
/// who would otherwise reach for `docker_v2.annotations`, which the validator
/// ignores (it reads `configFile.Config.Labels`, populated only by
/// `docker_v2.labels` / `--label`, not by OCI manifest annotations). Append
/// the anodizer-specific path so the rejection is fixable without spelunking
/// the registry source.
///
/// `server_name` is the rendered `mcp.name` so the hint can quote the exact
/// label value the validator demands.
fn oci_rejection_hint(response_body: &str, server_name: &str) -> String {
    if response_body.contains("io.modelcontextprotocol.server.name") {
        format!(
            " — the registry could not verify OCI image ownership: the published \
             image must carry the image config label \
             `io.modelcontextprotocol.server.name={server_name}`. Set it via \
             `docker_v2.labels` (NOT `annotations`, which the registry ignores) \
             or a Dockerfile `LABEL io.modelcontextprotocol.server.name=\"{server_name}\"`."
        )
    } else {
        String::new()
    }
}

fn publish_payload(
    publish_url: &str,
    body: &str,
    token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    name: &str,
    registry_url: &str,
) -> Result<()> {
    let client = build_client(Duration::from_secs(60))?;

    let request_body_len = body.len();

    // reqwest validates header values; CRLF in `token` surfaces as a send-error, not header injection.
    let (_, response_body) = retry_http_blocking(
        "mcp: POST /v0/publish",
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .post(publish_url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", token))
                .body(body.to_string())
                .send()
        },
        |status, response| {
            // Defense-in-depth: if the registry echoes our Authorization
            // header back in an error body, scrub the token before it
            // lands in the user-visible log. No evidence the registry
            // does this today, but the cost of redacting is one regex's
            // worth of CPU vs. the cost of leaking a token to logs.
            //
            // Include the request body length AND a bounded slice of
            // the response so 4xx schema rejections (e.g. the registry
            // tightening `body.packages[i].version`'s `minLength`) are
            // diagnosable from CI logs without a curl reproduction.
            let scrubbed = anodizer_core::redact::redact_bearer_tokens(response);
            let (snippet, truncated) = truncate_response_snippet(&scrubbed);
            // Surface the remediation hint off the full scrubbed body (not the
            // bounded snippet) so a marker pushed past the 512-byte cut is
            // still detected.
            let hint = oci_rejection_hint(&scrubbed, name);
            format!(
                "mcp: POST {} returned HTTP {} (request_body_len={}; response={}{}){}",
                publish_url, status, request_body_len, snippet, truncated, hint
            )
        },
    )
    .with_context(|| format!("mcp: publish to {}", publish_url))?;

    // Best-effort response parse — a malformed body is logged but not fatal,
    // since the upstream already confirmed success via 2xx.
    let status = serde_json::from_str::<ServerResponse>(&response_body)
        .ok()
        .and_then(|r| r.meta.official.map(|o| o.status))
        .unwrap_or_default();
    if status.is_empty() {
        log.status(&format!("mcp: published '{}' to {}", name, registry_url));
    } else {
        log.status(&format!(
            "mcp: published '{}' to {} (status={})",
            name, registry_url, status
        ));
    }
    Ok(())
}

/// Reset the experimental-warning flag. **Test-only** — production code never
/// resets the flag (the warning fires exactly once per process), but unit
/// tests assert the one-shot behaviour by invoking the publisher multiple
/// times.
#[cfg(test)]
pub(crate) fn reset_experimental_warned_for_test() {
    EXPERIMENTAL_WARNED.store(false, Ordering::SeqCst);
}

/// Cross-test serialization for the `EXPERIMENTAL_WARNED` static.
///
/// `warn_experimental_once` toggles a process-wide AtomicBool. Any test that
/// (a) resets that flag, or (b) invokes a code path that flips it (e.g.
/// `publish_with_registry` or `Publisher::run`), races with every other test
/// that does the same when cargo runs tests in parallel. Shared via this
/// module-level helper so both `tests.rs` and `publisher::publisher_tests`
/// use the same lock — without it, the per-module locks would not serialize
/// across modules and the warn-once assertion would race the run-end-to-end
/// tests.
#[cfg(test)]
pub(crate) fn warn_once_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
