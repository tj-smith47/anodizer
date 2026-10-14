use crate::publisher_helpers::count_noun;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking_deadline};
use anyhow::{Context as _, Result, anyhow, bail};

mod description;

use description::effective_description;
pub use description::resolve_full_description;

/// Serialized shape of a recorded DockerHub PATCH. One entry per
/// `(entry, image)` cell that was actually mutated this run.
///
/// `snapshot_description` / `snapshot_full_description` are the values
/// the repo carried BEFORE the PATCH, captured via a GET that runs
/// immediately before the mutation. `rollback()` re-authenticates and
/// PATCHes the snapshot back. A field that the GET response did not
/// carry (or carried as `null`) is recorded as `None` and omitted from
/// the rollback PATCH body, so an empty string the repo did not have is
/// never invented.
///
/// CREDENTIAL CONTRACT: this struct is the payload that ends up in
/// [`anodizer_core::PublishEvidence::extra`], which is persisted to
/// `dist/run-<id>/report.json` and may appear in the announce body.
/// `username` is operator-public (the DockerHub login appears on every
/// pushed image) and is recorded verbatim; `secret_env_var` records only
/// the NAME of the env var the rollback path re-resolves the password
/// from — never the password VALUE. Aliased to the core-owned snapshot so
/// the evidence schema lives in [`anodizer_core::publish_evidence`] and
/// resolved password values have no slot to fill.
type DockerhubTarget = anodizer_core::publish_evidence::DockerhubTargetSnapshot;

/// Decode the `dockerhub_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_dockerhub_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<DockerhubTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Dockerhub(d) => d.dockerhub_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve the Docker Hub API base URL through an injected env source.
///
/// Honors the undocumented `ANODIZER_DOCKERHUB_API_BASE` override so unit
/// tests can redirect the login / snapshot / PATCH calls to an in-process
/// responder via a [`MapEnvSource`](anodizer_core::MapEnvSource); defaults
/// to the canonical `https://hub.docker.com` in production where callers
/// pass [`anodizer_core::ProcessEnvSource`] and the var is unset. A
/// trailing `/` is stripped so the caller can append a `/`-prefixed suffix
/// without producing a double slash. Mirrors the
/// `ANODIZER_GITHUB_API_BASE` override used by the GitHub release backend.
fn dockerhub_api_base<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> String {
    env.var("ANODIZER_DOCKERHUB_API_BASE")
        .unwrap_or_else(|| "https://hub.docker.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The config half of the username ladder: `entry.username`, non-empty.
/// `DockerhubPublisher`'s [`Publisher::requirements`](anodizer_core::Publisher::requirements)
/// derives its env projection from
/// this same accessor, so the static requirement and the runtime ladder
/// key on one field read.
fn configured_username(entry: &anodizer_core::config::DockerHubConfig) -> Option<&str> {
    entry.username.as_deref().filter(|u| !u.is_empty())
}

/// Resolve the effective Docker Hub username for an entry: config value
/// (templated — rendered here, and a render failure is an `Err`, never a
/// raw template string passed on as a username) → `DOCKER_USERNAME` env →
/// `None`.
///
/// The one ladder shared by `run()` (bails on `None`) and `preflight()`
/// (maps `Err` to a Warning naming the render failure, so a template typo
/// is reported as a template problem; probing the login endpoint
/// with the literal template string would misreport a credential
/// failure). A config value that renders to the empty string is an `Err`
/// naming the template — the config explicitly owns the field when
/// present, so falling back to the env var (or reporting "'username' is
/// required" when it IS set) would misattribute the problem.
fn resolve_dockerhub_username(
    ctx: &Context,
    entry: &anodizer_core::config::DockerHubConfig,
) -> Result<Option<String>> {
    if let Some(u) = configured_username(entry) {
        let rendered = ctx
            .render_template(u)
            .with_context(|| format!("dockerhub: render username template {u:?}"))?;
        if rendered.is_empty() {
            bail!("dockerhub: username template {u:?} rendered to an empty string");
        }
        return Ok(Some(rendered));
    }
    Ok(ctx
        .env_var("DOCKER_USERNAME")
        .as_deref()
        .and_then(anodizer_core::git::non_blank_token))
}

/// Updates every configured repository description, writing each mutated repository into `targets`
/// as it is changed so a failure on a later entry still leaves the caller the
/// snapshots needed to restore the repositories changed before it.
fn publish_to_dockerhub_into(
    ctx: &Context,
    log: &StageLogger,
    targets: &mut Vec<DockerhubTarget>,
) -> Result<()> {
    let entries = match ctx.config.dockerhub {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    // One shared HTTP client for every entry: connection pool and TLS
    // handshakes are reused across repos.
    let shared_client = reqwest::blocking::Client::builder()
        .user_agent("anodizer/1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("dockerhub: failed to build shared HTTP client")?;

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every entry's full_description fetch, login, and PATCH.
    let policy = ctx.retry_policy();
    // One wall-clock budget for the whole dockerhub publish: the login, every
    // pre-PATCH snapshot GET and every PATCH share it, so a wedged API cannot
    // spend `retry.max_elapsed` once per request.
    let deadline = ctx.retry_deadline();

    let api_base = dockerhub_api_base(ctx.env_source());

    // JWT cache keyed by `(username, secret_env_name)`. When N entries
    // share the same login pair, a single authentication supplies the
    // bearer across PATCHes — saves API calls AND reduces the number
    // of times the secret value crosses the wire.
    let mut jwt_cache: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();

    for entry in entries {
        // Check skip flag.
        if let Some(ref d) = entry.skip {
            let off = d
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "dockerhub: render skip template")?;
            if off {
                log.status("skipped dockerhub entry — skip evaluates true");
                continue;
            }
        }

        // Check `if:` conditional gate.
        let proceed = anodizer_core::config::evaluate_if_condition(
            entry.if_condition.as_deref(),
            "dockerhub entry",
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped dockerhub entry — `if` condition evaluated falsy");
            continue;
        }

        // Resolve username from config, falling back to DOCKER_USERNAME.
        // Bail early when neither is set so config errors show even in
        // dry-run.
        let Some(username) = resolve_dockerhub_username(ctx, entry)? else {
            bail!("dockerhub: 'username' is required (set in config or via DOCKER_USERNAME env)")
        };

        // Render each image entry through the template engine. The docs
        // say image entries are templated; without this pass a config like
        // `images: ["{{ .Env.NAMESPACE }}/myapp"]` would be rejected as
        // malformed by the path-validation loop below.
        let raw_images = entry.images.as_deref().unwrap_or_default();
        let mut rendered_images: Vec<String> = Vec::with_capacity(raw_images.len());
        for image in raw_images {
            let rendered = ctx
                .render_template(image)
                .with_context(|| format!("dockerhub: render image '{}'", image))?;
            rendered_images.push(rendered);
        }
        let images: &[String] = &rendered_images;

        if images.is_empty() {
            ctx.strict_guard(log, "skipped dockerhub entry — no images configured")?;
            continue;
        }

        // Empty per-entry description falls back to the project's global
        // metadata.description so a single source of truth covers every
        // dockerhub entry. Same fallback chain as homebrew (cask +
        // formula), MCP, scoop, krew, etc.
        // `description` is a templated field, so render the resolved value — whether it
        // came from the entry or the metadata fallback — before it hits the
        // wire.
        let description_owned: Option<String> = match effective_description(entry, ctx) {
            Some(d) => Some(
                ctx.render_template(&d)
                    .with_context(|| "dockerhub: render description")?,
            ),
            None => None,
        };
        let short_desc: &str = description_owned.as_deref().unwrap_or("");
        // Docker Hub counts code points (UTF-8 bytes do not matter), so emoji /
        // accented descriptions don't falsely trip the 100-char warning.
        let short_desc_chars = short_desc.chars().count();
        if short_desc_chars > 100 {
            log.warn(&format!(
                "dockerhub short description is {} chars (max 100); Docker Hub will truncate it",
                short_desc_chars
            ));
        }

        // Validate image references: reject empty path segments and
        // multi-slash names; refuse bare names in strict mode (Docker Hub's
        // `library/` namespace requires Docker Inc permissions).
        for image in images {
            let segments: Vec<&str> = image.split('/').collect();
            let empty = segments.iter().filter(|s| s.is_empty()).count();
            if empty > 0 {
                bail!(
                    "dockerhub: image '{}' has {} (no leading/trailing slash, no consecutive slashes)",
                    image,
                    count_noun(empty, "empty path segment", "empty path segments")
                );
            }
            if segments.len() > 2 {
                bail!(
                    "dockerhub: image '{}' has too many path segments (Docker Hub format is 'namespace/repo')",
                    image
                );
            }
            if !image.contains('/') {
                ctx.strict_guard(log, &format!(
                    "dockerhub image '{}' has no namespace; bare names map to 'library/' which requires Docker Inc permissions",
                    image
                ))?;
            }
        }

        // Report a missing secret env in dry-run as a warning (live mode
        // will hard-fail later) so a typo'd secret_name doesn't slip
        // through unnoticed during config-test runs.
        // A templated `secret_name` (`TOKEN_{{ .Env.STAGE }}`) must be
        // rendered before the env lookup, or the literal template string is
        // looked up, never found, and the credential is silently treated as
        // absent on a live publish.
        let secret_name = crate::util::resolve_secret_name(
            ctx,
            "dockerhub.secret_name",
            entry.secret_name.as_deref(),
            "DOCKER_PASSWORD",
        )?;
        if ctx.is_dry_run()
            && ctx
                .env_var(&secret_name)
                .as_deref()
                .and_then(anodizer_core::git::non_blank_token)
                .is_none()
        {
            log.warn(&format!(
                "dockerhub secret env '{}' is unset or blank; live mode will fail authentication",
                secret_name
            ));
        }

        // Dry-run: log the intent without resolving full_description (which
        // would do file/network I/O).
        if ctx.is_dry_run() {
            for image in images {
                log.status(&format!(
                    "(dry-run) would sync DockerHub description for '{}'",
                    image
                ));
            }
            continue;
        }

        let client = &shared_client;

        // Resolve full description if configured (after dry-run check).
        let full_desc = match entry.full_description {
            Some(ref fd) => Some(
                resolve_full_description(fd, ctx, client, &policy, log)
                    .context("dockerhub: failed to resolve full_description")?,
            ),
            None => None,
        };

        // Skip PATCH when both descriptions are absent — there's nothing
        // to sync and Docker Hub would clobber the existing description
        // with empty strings.
        if short_desc.is_empty() && full_desc.is_none() {
            ctx.strict_guard(
                log,
                "skipped dockerhub description PATCH — both description and full_description are empty",
            )?;
            continue;
        }

        // Authenticate: POST to get JWT token. Reuse a cached JWT when
        // multiple entries share the same (username, secret_env_name)
        // pair so the login round-trip is not paid per entry.
        let cache_key = (username.clone(), secret_name.to_string());
        let token = if let Some(cached) = jwt_cache.get(&cache_key) {
            cached.clone()
        } else {
            let password = ctx
                .env_var(&secret_name)
                .as_deref()
                .and_then(anodizer_core::git::non_blank_token)
                .ok_or_else(|| {
                    anyhow!("dockerhub: environment variable '{}' not set", secret_name)
                })?;

            let login_body = serde_json::json!({
                "username": username,
                "password": password,
            });

            let (_, login_body_text) = retry_http_blocking_deadline(
                RetryLog::new("dockerhub: authenticate", log),
                &policy,
                deadline,
                SuccessClass::Strict,
                |_| {
                    client
                        .post(format!("{api_base}/v2/users/login/"))
                        .json(&login_body)
                        .send()
                },
                |status, body| {
                    format!(
                        "dockerhub: authentication failed (HTTP {status}): {}",
                        redact_bearer_tokens(body)
                    )
                },
            )?;

            let login_json: serde_json::Value = serde_json::from_str(&login_body_text)
                .context("dockerhub: failed to parse login response")?;

            let token_str = login_json["token"]
                .as_str()
                .ok_or_else(|| anyhow!("dockerhub: no token in login response"))?
                .to_string();
            jwt_cache.insert(cache_key.clone(), token_str.clone());
            token_str
        };
        let token = token.as_str();

        // PATCH each image repository.
        for image in images {
            let parts: Vec<&str> = image.splitn(2, '/').collect();
            let (namespace, name) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                ("library", parts[0])
            };

            let repo_url = format!("{}/v2/repositories/{}/{}/", api_base, namespace, name);

            // Snapshot the prior description + full_description BEFORE
            // mutating, so rollback can restore them. The GET uses the
            // same JWT — DockerHub treats GET on a public repo as
            // anonymous-safe but a private repo requires auth, so
            // sending the bearer covers both cases. Failure to read
            // the snapshot is a failure of the publish itself: an unread
            // snapshot means rollback cannot be honored, and proceeding
            // would silently degrade the rollback contract.
            let snapshot_label = format!("dockerhub: GET snapshot for {}", image);
            let (_, snapshot_body) = retry_http_blocking_deadline(
                RetryLog::new(&snapshot_label, log),
                &policy,
                deadline,
                SuccessClass::Strict,
                |_| client.get(&repo_url).bearer_auth(token).send(),
                |status, body| {
                    format!(
                        "dockerhub: GET {} snapshot failed (HTTP {}): {}",
                        image,
                        status,
                        redact_bearer_tokens(body)
                    )
                },
            )?;
            let snapshot_json: serde_json::Value = serde_json::from_str(&snapshot_body)
                .with_context(|| {
                    format!(
                        "dockerhub: failed to parse snapshot response for '{}'",
                        image
                    )
                })?;
            let snapshot_description = snapshot_json
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let snapshot_full_description = snapshot_json
                .get("full_description")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            // Content-hash idempotency: skip the PATCH entirely when the
            // snapshot already matches the values to send. Saves an API
            // call AND keeps the Docker Hub audit log free of no-op
            // "description updated" entries.
            let short_unchanged = if short_desc.is_empty() {
                true
            } else {
                snapshot_description.as_deref() == Some(short_desc)
            };
            let full_unchanged = match (&snapshot_full_description, &full_desc) {
                (_, None) => true,
                (Some(prev), Some(new)) => prev == new,
                (None, Some(new)) => new.is_empty(),
            };
            if short_unchanged && full_unchanged {
                log.status(&format!(
                    "skipped dockerhub PATCH for '{}' — description / full_description match remote",
                    image
                ));
                continue;
            }

            let mut patch_body = serde_json::Map::new();
            if !short_desc.is_empty() {
                patch_body.insert(
                    "description".to_string(),
                    serde_json::Value::String(short_desc.to_string()),
                );
            }
            if let Some(ref fd) = full_desc {
                patch_body.insert(
                    "full_description".to_string(),
                    serde_json::Value::String(fd.clone()),
                );
            }

            let label = format!("dockerhub: PATCH {}", image);
            retry_http_blocking_deadline(
                RetryLog::new(&label, log),
                &policy,
                deadline,
                SuccessClass::Strict,
                |_| {
                    client
                        .patch(&repo_url)
                        .bearer_auth(token)
                        .json(&patch_body)
                        .send()
                },
                |status, body| {
                    format!(
                        "dockerhub: PATCH {}/{} failed (HTTP {}): {}",
                        namespace,
                        name,
                        status,
                        redact_bearer_tokens(body)
                    )
                },
            )?;

            log.status(&format!("synced dockerhub description for '{}'", image));
            targets.push(DockerhubTarget {
                target: image.clone(),
                repo_url,
                namespace: namespace.to_string(),
                name: name.to_string(),
                username: username.clone(),
                secret_env_var: secret_name.to_string(),
                snapshot_description,
                snapshot_full_description,
            });
        }
    }

    Ok(())
}

/// Re-authenticate to DockerHub and PATCH a single target back to its
/// snapshotted `description` / `full_description`. Used exclusively by
/// `DockerhubPublisher`'s [`Publisher::rollback`](anodizer_core::Publisher::rollback).
///
/// Credentials are re-resolved at rollback time from the injected env
/// source — production wires up [`anodizer_core::ProcessEnvSource`];
/// tests inject a [`anodizer_core::MapEnvSource`] so the missing-env
/// branch can be exercised without mutating the process env. The
/// password value never crosses
/// [`PublishEvidence::extra`](anodizer_core::PublishEvidence::extra). A target
/// whose snapshot was entirely `None` (rare — neither field was
/// readable at publish time) is a no-op: there is nothing to restore.
fn restore_dockerhub_target_with_env<E: anodizer_core::EnvSource + ?Sized>(
    client: &reqwest::blocking::Client,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    target: &DockerhubTarget,
    env: &E,
    log: &StageLogger,
) -> Result<()> {
    let mut patch_body = serde_json::Map::new();
    if let Some(ref d) = target.snapshot_description {
        patch_body.insert(
            "description".to_string(),
            serde_json::Value::String(d.clone()),
        );
    }
    if let Some(ref fd) = target.snapshot_full_description {
        patch_body.insert(
            "full_description".to_string(),
            serde_json::Value::String(fd.clone()),
        );
    }
    if patch_body.is_empty() {
        return Ok(());
    }

    let password = env
        .var(&target.secret_env_var)
        .as_deref()
        .and_then(anodizer_core::git::non_blank_token)
        .ok_or_else(|| {
            anyhow!(
                "dockerhub: env var '{}' is unset or blank; cannot re-authenticate to restore '{}'",
                target.secret_env_var,
                target.target
            )
        })?;

    let login_body = serde_json::json!({
        "username": &target.username,
        "password": password,
    });
    let login_url = format!("{}/v2/users/login/", dockerhub_api_base(env));
    let (_, login_body_text) = retry_http_blocking_deadline(
        RetryLog::new("dockerhub: rollback authenticate", log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| client.post(&login_url).json(&login_body).send(),
        |status, body| {
            format!(
                "dockerhub: rollback authentication failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )?;
    let login_json: serde_json::Value = serde_json::from_str(&login_body_text)
        .context("dockerhub: failed to parse rollback login response")?;
    let token = login_json["token"]
        .as_str()
        .ok_or_else(|| anyhow!("dockerhub: no token in rollback login response"))?;

    let label = format!("dockerhub: rollback PATCH {}", target.target);
    retry_http_blocking_deadline(
        RetryLog::new(&label, log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| {
            client
                .patch(&target.repo_url)
                .bearer_auth(token)
                .json(&patch_body)
                .send()
        },
        |status, body| {
            format!(
                "dockerhub: rollback PATCH {} failed (HTTP {}): {}",
                target.target,
                status,
                redact_bearer_tokens(body)
            )
        },
    )?;
    Ok(())
}

// Wraps [`publish_to_dockerhub`] in the [`anodizer_core::Publisher`] trait so
// the dispatch path (see [`crate::registry::configured_publishers`]) can
// drive Docker Hub description sync alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (description sync is a
// best-effort publisher; not required for the release to succeed).
//
// Rollback shape: DockerHub publishes a PATCH against the repo's
// `description` and `full_description` fields. Before each PATCH the
// publish path issues a GET to snapshot the prior values into
// [`DockerhubTarget`]; the rollback path re-authenticates (via the
// captured `secret_env_var`) and PATCHes the snapshot back. The
// `<password>` value is never persisted — only the env var name is —
// matching the credential contract enforced for every other
// `*Target` evidence struct.
simple_publisher!(
    DockerhubPublisher,
    "dockerhub",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("DOCKER_PASSWORD description snapshot+restore"),
);

/// Top-level `dockerhub:` entries whose `skip:`/`if:` evaluates active
/// right now. Shared by [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `preflight` keeps its own loop (it needs per-entry repository
/// resolution alongside the filter, beyond a boolean).
fn active_dockerhub_configs(ctx: &Context) -> Vec<&anodizer_core::config::DockerHubConfig> {
    ctx.config
        .dockerhub
        .iter()
        .flatten()
        .filter(|entry| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            )
        })
        .collect()
}

/// The evidence for the repositories `targets` names. `artifact_paths`
/// indexes every repo this run actually mutated (driven off the targets, not
/// config) so dry-run / skip paths do not leak phantom entries; `primary_ref`
/// points at the first mutated repo for log-line continuity.
fn dockerhub_evidence(targets: Vec<DockerhubTarget>) -> anodizer_core::PublishEvidence {
    let mut evidence = anodizer_core::PublishEvidence::new("dockerhub");
    let paths: Vec<std::path::PathBuf> = targets
        .iter()
        .map(|t| std::path::PathBuf::from(&t.repo_url))
        .collect();
    if let Some(first) = paths.first() {
        evidence.primary_ref = Some(first.display().to_string());
    }
    evidence.artifact_paths = paths;
    if !targets.is_empty() {
        evidence.extra = anodizer_core::PublishEvidenceExtra::Dockerhub(
            anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: targets,
            },
        );
    }
    evidence
}

impl anodizer_core::Publisher for DockerhubPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_dockerhub_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let mut out = Vec::new();
        for entry in active_dockerhub_configs(ctx) {
            // Static projection of [`resolve_dockerhub_username`]'s ladder,
            // keyed on the same [`configured_username`] accessor: password
            // from `secret_name` (default DOCKER_PASSWORD); username env
            // refs when the config value is templated, the DOCKER_USERNAME
            // fallback when the config value is absent.
            // A `secret_name` that does not render is reported by the
            // publish path; here it falls back to the default name.
            let secret = crate::util::resolve_secret_name(
                ctx,
                "dockerhub.secret_name",
                entry.secret_name.as_deref(),
                "DOCKER_PASSWORD",
            )
            .unwrap_or_else(|_| "DOCKER_PASSWORD".to_string());
            out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: vec![secret] });
            match configured_username(entry) {
                Some(u) => {
                    let refs = anodizer_core::env_preflight::template_env_refs(u);
                    if !refs.is_empty() {
                        out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
                    }
                }
                None => out.push(anodizer_core::EnvRequirement::EnvAllOf {
                    vars: vec!["DOCKER_USERNAME".to_string()],
                }),
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets = Vec::new();
        let synced = publish_to_dockerhub_into(ctx, &log, &mut targets);
        crate::publisher_helpers::keep_committed_on_failure(ctx, synced, |_| {
            (!targets.is_empty()).then(|| dockerhub_evidence(targets.clone()))
        })?;
        Ok(dockerhub_evidence(targets))
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_dockerhub_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                ctx,
                "dockerhub",
                "description-sync targets",
            ));
            return Ok(());
        }

        let client = match reqwest::blocking::Client::builder()
            .user_agent("anodizer/1.0")
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                // Building a reqwest client only fails on a malformed
                // TLS config — vanishingly unlikely, but it degrades to
                // the warn-only checklist; bubbling Err would gate
                // rollback of sibling publishers.
                log.warn(&format!(
                    "dockerhub rollback could not build HTTP client ({e:#}); \
                     manual review required for {}",
                    count_noun(targets.len(), "repo", "repos")
                ));
                for t in &targets {
                    log.warn(&format!(
                        "manual dockerhub restore needed for {} ({})",
                        t.target, t.repo_url
                    ));
                }
                return Ok(());
            }
        };
        let policy = ctx.retry_policy();
        let deadline = ctx.retry_deadline();
        let env = ctx.env_source();

        let mut restored = 0usize;
        let mut failed = 0usize;
        for t in &targets {
            match restore_dockerhub_target_with_env(&client, &policy, deadline, t, env, &log) {
                Ok(()) => {
                    restored += 1;
                    log.status(&format!(
                        "restored dockerhub description for '{}'",
                        t.target
                    ));
                }
                Err(e) => {
                    failed += 1;
                    log.warn(&format!(
                        "failed to restore dockerhub description for '{}' ({}): {:#}; \
                         check ${} is set or restore manually via the Docker Hub UI",
                        t.target, t.repo_url, e, t.secret_env_var
                    ));
                }
            }
        }
        log.status(&format!(
            "dockerhub rollback restored {}, {}",
            count_noun(restored, "description", "descriptions"),
            count_noun(failed, "failure", "failures")
        ));
        Ok(())
    }

    /// Live pre-publish gate. For every active `dockerhub[]` entry whose login
    /// credentials resolve, POST `{api_base}/v2/users/login/` with
    /// `{username, password}` — the exact login the publish path performs to create
    /// its JWT. A rejected login (401/403) or an unreachable host is reported as a
    /// Warning (this publisher is OPTIONAL — a failed description sync must not
    /// abort the release). Logins are de-duplicated per `(username, secret_name)`
    /// so N entries sharing one credential probe once.
    ///
    /// Entries with no resolvable credential are left to `requirements()` (which
    /// gates credential *presence*); this probe only proves a *present* login
    /// is accepted.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{
            FailSeverity, ProbeAuth, ProbeMethod, default_probe_client, merge, probe_http_endpoint,
        };
        use anodizer_core::PreflightCheck;
        use std::collections::HashSet;

        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let fail = FailSeverity::for_required(Self::resolved_required(self));
        let api_base = dockerhub_api_base(ctx.env_source());
        let login_url = format!("{api_base}/v2/users/login/");

        let mut acc = PreflightCheck::Pass;
        let mut probed: HashSet<(String, String)> = HashSet::new();
        for entry in ctx.config.dockerhub.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            // Same ladder `run()` uses; a render failure becomes a Warning
            // naming the template problem without probing the login
            // endpoint with the raw template string (which would misreport
            // a template typo as a credential failure).
            let username = match resolve_dockerhub_username(ctx, entry) {
                Ok(u) => u,
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "dockerhub: username template did not render ({e:#}); \
                             login probe skipped — fix the template",
                        )),
                    );
                    continue;
                }
            };
            let secret_name = match crate::util::resolve_secret_name(
                ctx,
                "dockerhub.secret_name",
                entry.secret_name.as_deref(),
                "DOCKER_PASSWORD",
            ) {
                Ok(name) => name,
                Err(e) => {
                    acc = merge(acc, PreflightCheck::Blocker(format!("dockerhub: {e:#}")));
                    continue;
                }
            };
            // An unresolved credential is `requirements()`'s job to flag; this
            // probe only validates a credential that IS present.
            let (Some(username), Some(password)) = (
                username,
                ctx.env_var(&secret_name)
                    .as_deref()
                    .and_then(anodizer_core::git::non_blank_token),
            ) else {
                continue;
            };
            if !probed.insert((username.clone(), secret_name.to_string())) {
                continue;
            }
            let client = match default_probe_client() {
                Ok(c) => c,
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "dockerhub: could not build HTTP client for preflight ({e})"
                        )),
                    );
                    continue;
                }
            };
            let body = serde_json::json!({ "username": username, "password": password });
            acc = merge(
                acc,
                probe_http_endpoint(
                    &client,
                    ProbeMethod::PostJson(body),
                    &login_url,
                    &ProbeAuth::None,
                    "preflight: dockerhub login",
                    fail,
                    ctx.preflight_is_strict(),
                    &policy,
                    ctx.retry_deadline(),
                    &ctx.logger("preflight"),
                ),
            );
        }
        Ok(acc)
    }

    fn skips_on_nightly(&self) -> bool {
        // Docker registries accept tag rewrites; nightly clobber is intentional.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}

/// Sync descriptions to Docker Hub repositories.
///
/// This is a top-level publisher: it reads from `ctx.config.dockerhub` rather
/// than from per-crate publish configs.
///
/// Returns one [`DockerhubTarget`] per repository the PATCH actually
/// mutated. Each target carries the pre-PATCH `description` and
/// `full_description` snapshot (captured via a GET that runs
/// immediately before the mutation) so the
/// [`Publisher::rollback`](anodizer_core::Publisher::rollback)
/// path can re-authenticate and restore the prior values. Dry-run,
/// skipped entries, and configurations that short-circuit the PATCH
/// (empty descriptions) produce no targets.
#[cfg(test)]
fn publish_to_dockerhub(ctx: &Context, log: &StageLogger) -> Result<Vec<DockerhubTarget>> {
    let mut targets: Vec<DockerhubTarget> = Vec::new();
    publish_to_dockerhub_into(ctx, log, &mut targets)?;
    Ok(targets)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
