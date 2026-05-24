//! MCP (Model Context Protocol) server registry publisher.
//!
//! Posts an `apiv0.ServerJSON` document to `{registry}/v0/publish` after
//! exchanging the configured credentials for a registry JWT. Mirrors
//! GoReleaser `internal/pipe/mcp/mcp.go` end-to-end, with two intentional
//! deviations:
//!
//! 1. **No on-disk token files.** GR writes `.mcpregistry_github_token` and
//!    `.mcpregistry_registry_token` to the cwd and deletes them post-
//!    publish. Anodizer keeps tokens in memory — same wire behaviour, fewer
//!    artefacts left behind on disk if the process dies mid-publish.
//! 2. **Defaulting collapsed.** GR's `Default(ctx)` migrates a deprecated
//!    `mcp.github:` block to the top-level fields and defaults
//!    `auth.type=none`. Anodizer never had the nested form, and our
//!    `McpAuth::default()` already defaults to `None`, so there's no
//!    separate `Default` phase here.
//!
//! Skip gate (matches GR `mcp.go::Skip`):
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
use serde::{Deserialize, Serialize};

use anodizer_core::config::{McpAuthMethod, McpConfig};

use auth::{build_client, provider_for};
use manifest::{
    CURRENT_SCHEMA_URL, DEFAULT_REGISTRY_URL, Package, Repository, ServerJson, ServerResponse,
    Transport,
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
/// NB: no `token`, `password`, or `pat` fields — see [`publisher`] module
/// rustdoc for the credential-handling rationale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct McpTarget {
    /// Per-target label — duplicates `server_name` for log-line shape
    /// parity with the krew/homebrew/scoop publishers.
    pub(crate) target: String,
    /// Fully-qualified MCP server name in reverse-DNS form
    /// (e.g. `io.github.user/weather`).
    pub(crate) server_name: String,
    /// Resolved registry endpoint the publish path posted to.
    pub(crate) registry_url: String,
    /// Version string published (`ctx.version()` at publish time).
    pub(crate) version: String,
    /// Auth method in use — determines which provider rollback builds.
    /// Stored as the enum (serializes as `"none"` / `"github"` /
    /// `"github-oidc"`) so rollback re-authenticates identically to publish.
    pub(crate) auth_method: McpAuthMethod,
}

/// Process-wide flag — the "mcp is experimental" warning is emitted at
/// most once per anodizer invocation regardless of how many crates trigger
/// the publisher. Matches GR's `warnExperimental` which uses
/// `caarlos0/log`'s side-effect-on-construction pattern (the deprecation
/// notice is similarly one-shot).
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
pub(crate) fn publish_to_mcp(ctx: &Context, log: &StageLogger) -> Result<Option<McpTarget>> {
    let registry_url = resolve_registry_url(&ctx.config.mcp);
    publish_with_registry(ctx, log, registry_url)
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
    ctx: &Context,
    log: &StageLogger,
    registry_url: &str,
) -> Result<Option<McpTarget>> {
    let mcp = &ctx.config.mcp;

    // ---- Skip gate (GR mcp.go::Skip parity) ----
    if mcp.name.as_deref().unwrap_or("").is_empty() {
        log.status("mcp: skipping — no mcp.name configured");
        return Ok(None);
    }
    if let Some(skip) = mcp.skip.as_ref() {
        let off = skip
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("mcp: render skip template")?;
        if off {
            log.status("mcp: skipping — skip evaluates true");
            return Ok(None);
        }
    }

    // ---- One-shot experimental warning ----
    warn_experimental_once(log);

    // ---- Template-render every string field (GR mcp.go:72-85 parity) ----
    let mut mcp_rendered: McpConfig = mcp.clone();
    render_strings(ctx, &mut mcp_rendered)?;
    infer_repository_from_release(ctx, &mut mcp_rendered);

    let rendered_name = mcp_rendered.name.clone().unwrap_or_default();

    // ---- Dry-run short-circuits before any network I/O ----
    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish to MCP registry {} as '{}' (auth={})",
            registry_url,
            rendered_name,
            mcp_rendered.auth.method.as_str()
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

    // ---- Assemble the ServerJSON payload ----
    let server = build_server_json(&mcp_rendered, &ctx.version());
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

/// Fill in `mcp.repository.url` / `mcp.repository.source` from the configured
/// `release.<github|gitlab|gitea>` block when the user left them blank. This
/// matches the doc claim that anodizer "infers `url` and `source` from the
/// release context" — without this, the entire `repository` object is
/// silently omitted from the published payload (since the builder gates on
/// `mcp.repository.url.is_empty()`).
fn infer_repository_from_release(ctx: &Context, mcp: &mut McpConfig) {
    if !mcp.repository.url.is_empty() {
        return;
    }
    let Some(release) = ctx.config.release.as_ref() else {
        return;
    };
    let (host, repo) = if let Some(r) = release.github.as_ref() {
        ("github", r)
    } else if let Some(r) = release.gitlab.as_ref() {
        ("gitlab", r)
    } else if let Some(r) = release.gitea.as_ref() {
        ("gitea", r)
    } else {
        return;
    };
    if repo.owner.is_empty() || repo.name.is_empty() {
        return;
    }
    let base = match host {
        "github" => "https://github.com",
        "gitlab" => "https://gitlab.com",
        "gitea" => "https://gitea.com",
        _ => return,
    };
    mcp.repository.url = format!("{}/{}/{}", base, repo.owner, repo.name);
    if mcp.repository.source.is_empty() {
        mcp.repository.source = host.to_string();
    }
}

/// Emit the experimental-warning banner the first time the publisher runs in
/// this process; subsequent invocations are silent. The atomic flag is
/// process-wide (matches GR's package-level `sync.Once`-ish behaviour for
/// log lines emitted via `caarlos0/log`).
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
/// Mirrors GR `mcp.go::Publish` lines 72-85 (the `tmpl.New(ctx).ApplyAll(&mcp.Name,
/// &mcp.Description, ...)` block) AND the per-package render at lines 127-129
/// (`tmpl.New(ctx).ApplyAll(&pkg.Identifier)`). One-stop helper so the
/// publisher orchestration stays linear.
fn render_strings(ctx: &Context, mcp: &mut McpConfig) -> Result<()> {
    render_opt_in_place(ctx, &mut mcp.name, "name")?;
    render_opt_in_place(ctx, &mut mcp.description, "description")?;
    render_opt_in_place(ctx, &mut mcp.title, "title")?;
    render_opt_in_place(ctx, &mut mcp.homepage, "homepage")?;
    render_in_place(ctx, &mut mcp.repository.url, "repository.url")?;
    render_in_place(ctx, &mut mcp.repository.source, "repository.source")?;
    render_in_place(ctx, &mut mcp.repository.id, "repository.id")?;
    render_in_place(ctx, &mut mcp.repository.subfolder, "repository.subfolder")?;

    // auth.type: GR renders this through tmpl.ApplyAll despite it being an
    // enum on its config struct (Go uses a string type). We mirror the
    // behaviour by rendering the wire-format string and re-parsing — so a
    // user can write `auth.type: "{{ if eq .Env.MODE \"ci\" }}github-oidc{{ else }}none{{ end }}"`
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
/// Mirrors GR `mcp.go::Publish` lines 108-144:
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
/// logs `published to MCP registry name=<...> status=<...>` — matching GR
/// `mcp.go:181-184`'s log shape.
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
        |status, body| {
            // Defense-in-depth: if the registry echoes our Authorization
            // header back in an error body, scrub the token before it
            // lands in the user-visible log. No evidence the registry
            // does this today, but the cost of redacting is one regex's
            // worth of CPU vs. the cost of leaking a token to logs.
            format!(
                "mcp: POST {} returned HTTP {}: {}",
                publish_url,
                status,
                anodizer_core::redact::redact_bearer_tokens(body)
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
/// resets the flag (we want the warning to fire exactly once per process,
/// per the GR parity rule), but unit tests assert the one-shot behaviour by
/// invoking the publisher multiple times.
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
