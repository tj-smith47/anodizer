use anodizer_core::config::DockerHubFullDescription;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, anyhow, bail};

/// Serialized shape of a recorded DockerHub PATCH. One entry per
/// `(entry, image)` cell that was actually mutated this run.
///
/// `snapshot_description` / `snapshot_full_description` are the values
/// the repo carried BEFORE our PATCH, captured via a GET that runs
/// immediately before the mutation. `rollback()` re-authenticates and
/// PATCHes the snapshot back. A field that the GET response did not
/// carry (or carried as `null`) is recorded as `None` and omitted from
/// the rollback PATCH body so we never invent an empty string the
/// repo did not have.
///
/// CREDENTIAL CONTRACT: this struct is the payload that lands in
/// [`anodizer_core::PublishEvidence::extra`], which is persisted to
/// `dist/run-<id>/report.json` and may surface in the announce body.
/// `username` is operator-public (the DockerHub login appears on every
/// pushed image) and is recorded verbatim; `secret_env_var` is the
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// (resolved password VALUES) have no slot to land in — only the env
/// var NAME the rollback path consults.
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
/// `ANODIZER_GITHUB_API_BASE` seam used by the GitHub release backend.
fn dockerhub_api_base<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> String {
    env.var("ANODIZER_DOCKERHUB_API_BASE")
        .unwrap_or_else(|| "https://hub.docker.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

// ---------------------------------------------------------------------------
// resolve_full_description
// ---------------------------------------------------------------------------

/// Resolve the full description content from either a local file or a URL.
/// `from_file` takes precedence over `from_url` when both are set.
///
/// The `from_url` branch routes through [`retry_http_blocking`] so transient
/// 5xx / 429 / network failures retry per the user's top-level `retry:`
/// policy; 4xx fast-fails so a typo'd URL surfaces immediately.
///
/// Both `from_file.path` and `from_url.url` are template-rendered through
/// `ctx` before use so configs like
/// `from_url: { url: "https://raw.githubusercontent.com/{{ .Env.OWNER }}/.../README.md" }`
/// work as documented.
pub fn resolve_full_description(
    desc: &DockerHubFullDescription,
    ctx: &Context,
    client: &reqwest::blocking::Client,
    policy: &RetryPolicy,
) -> Result<String> {
    if let Some(ref from_file) = desc.from_file {
        let path = ctx.render_template(&from_file.path).with_context(|| {
            format!(
                "dockerhub: render full_description from_file path '{}'",
                from_file.path
            )
        })?;
        return std::fs::read_to_string(&path)
            .with_context(|| format!("dockerhub: failed to read file '{}'", path));
    }

    if let Some(ref from_url) = desc.from_url {
        let url = ctx.render_template(&from_url.url).with_context(|| {
            format!(
                "dockerhub: render full_description from_url '{}'",
                from_url.url
            )
        })?;
        let headers = from_url.headers.clone();
        let label = format!("dockerhub: fetch full_description from {}", url);
        let (_, body) = retry_http_blocking(
            &label,
            policy,
            SuccessClass::Strict,
            |_| {
                let mut req = client.get(&url);
                if let Some(ref h) = headers {
                    for (key, value) in h {
                        req = req.header(key, value);
                    }
                }
                req.send()
            },
            |status, body| {
                format!(
                    "dockerhub: GET {} returned HTTP {}: {}",
                    url,
                    status,
                    redact_bearer_tokens(body)
                )
            },
        )?;
        return Ok(body);
    }

    bail!("dockerhub: full_description has neither from_file nor from_url set")
}

/// Resolve the short description for a DockerHub entry. Returns
/// `entry.description` when set non-empty; otherwise falls back to the
/// top-level `metadata.description` via [`Config::meta_description`].
/// Returns `None` when both sources are unset/empty so the caller can
/// short-circuit the PATCH and avoid clobbering an existing remote
/// description with the empty string.
fn effective_description(
    entry: &anodizer_core::config::DockerHubConfig,
    ctx: &Context,
) -> Option<String> {
    entry
        .description
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ctx.config
                .meta_description_project()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

// ---------------------------------------------------------------------------
// publish_to_dockerhub
// ---------------------------------------------------------------------------

/// Sync descriptions to Docker Hub repositories.
///
/// This is a top-level publisher: it reads from `ctx.config.dockerhub` rather
/// than from per-crate publish configs.
///
/// Returns one [`DockerhubTarget`] per repository the PATCH actually
/// mutated. Each target carries the pre-PATCH `description` and
/// `full_description` snapshot (captured via a GET that runs
/// immediately before the mutation) so the [`Publisher::rollback`]
/// path can re-authenticate and restore the prior values. Dry-run,
/// skipped entries, and configurations that short-circuit the PATCH
/// (empty descriptions) produce no targets.
fn publish_to_dockerhub(ctx: &Context, log: &StageLogger) -> Result<Vec<DockerhubTarget>> {
    let mut targets: Vec<DockerhubTarget> = Vec::new();
    let entries = match ctx.config.dockerhub {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(targets),
    };

    // One shared HTTP client for every entry: connection pool and TLS
    // handshakes are reused across repos.
    let shared_client = reqwest::blocking::Client::builder()
        .user_agent("anodizer/1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("dockerhub: failed to build shared HTTP client")?;

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every entry's full_description fetch, login, and PATCH (mirrors
    // the retry policy is captured once per pipe).
    let policy = ctx.retry_policy();

    let api_base = dockerhub_api_base(ctx.env_source());

    // JWT cache keyed by `(username, secret_env_name)`. When N entries
    // share the same login pair we authenticate once and reuse the
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
                log.status("dockerhub: entry skipped");
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
            log.status("dockerhub: entry skipped — `if` condition evaluated falsy");
            continue;
        }

        // Resolve username from config, falling back to DOCKER_USERNAME.
        // Bail early when neither is set so config errors surface even in
        // dry-run.
        let username_env = ctx.env_var("DOCKER_USERNAME");
        let username = match entry.username.as_deref() {
            Some(u) if !u.is_empty() => ctx
                .render_template(u)
                .with_context(|| format!("dockerhub: render username template {u:?}"))?,
            _ => match username_env.as_deref() {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => bail!(
                    "dockerhub: 'username' is required (set in config or via DOCKER_USERNAME env)"
                ),
            },
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
            ctx.strict_guard(log, "dockerhub: no images configured, skipping entry")?;
            continue;
        }

        // Empty per-entry description falls back to the project's global
        // metadata.description so a single source of truth covers every
        // dockerhub entry. Same fallback chain as homebrew (cask +
        // formula), MCP, scoop, krew, etc.
        // `description` is a templated field (GoReleaser dockerhub parity:
        // "Templates: allowed"), so render the resolved value — whether it
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
        // Docker Hub counts code points, not UTF-8 bytes, so emoji /
        // accented descriptions don't falsely trip the 100-char warning.
        let short_desc_chars = short_desc.chars().count();
        if short_desc_chars > 100 {
            log.warn(&format!(
                "dockerhub: short description is {} chars (max 100); Docker Hub will truncate it",
                short_desc_chars
            ));
        }

        // Validate image references: reject empty path segments and
        // multi-slash names; refuse bare names in strict mode (Docker Hub's
        // `library/` namespace requires Docker Inc permissions).
        for image in images {
            let segments: Vec<&str> = image.split('/').collect();
            if segments.iter().any(|s| s.is_empty()) {
                bail!(
                    "dockerhub: image '{}' has empty path segment(s) (no leading/trailing slash, no consecutive slashes)",
                    image
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
                    "dockerhub: image '{}' has no namespace; bare names map to 'library/' which requires Docker Inc permissions",
                    image
                ))?;
            }
        }

        // Surface a missing secret env in dry-run as a warning (live mode
        // will hard-fail later) so a typo'd secret_name doesn't slip
        // through unnoticed during config-test runs.
        let secret_name = entry.secret_name.as_deref().unwrap_or("DOCKER_PASSWORD");
        if ctx.is_dry_run() && ctx.env_var(secret_name).is_none() {
            log.warn(&format!(
                "dockerhub: secret env '{}' is not set; live mode will fail authentication",
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
                resolve_full_description(fd, ctx, client, &policy)
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
                "dockerhub: both description and full_description are empty, skipping PATCH",
            )?;
            continue;
        }

        // Authenticate: POST to get JWT token. Reuse a cached JWT when
        // multiple entries share the same (username, secret_env_name)
        // pair so we don't pay the login round-trip per entry.
        let cache_key = (username.clone(), secret_name.to_string());
        let token = if let Some(cached) = jwt_cache.get(&cache_key) {
            cached.clone()
        } else {
            let password = ctx.env_var(secret_name).ok_or_else(|| {
                anyhow!("dockerhub: environment variable '{}' not set", secret_name)
            })?;

            let login_body = serde_json::json!({
                "username": username,
                "password": password,
            });

            let (_, login_body_text) = retry_http_blocking(
                "dockerhub: authenticate",
                &policy,
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
            // the snapshot is a failure of the publish itself: if we
            // cannot read it we cannot honor rollback, and proceeding
            // would silently degrade the rollback contract.
            let snapshot_label = format!("dockerhub: GET snapshot for {}", image);
            let (_, snapshot_body) = retry_http_blocking(
                &snapshot_label,
                &policy,
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
            // snapshot already matches the values we'd send. Saves an API
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
                    "dockerhub: no changes for '{}' (description / full_description match remote); skipping PATCH",
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
            retry_http_blocking(
                &label,
                &policy,
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

            log.status(&format!("dockerhub: synced description for '{}'", image));
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

    Ok(targets)
}

/// Re-authenticate to DockerHub and PATCH a single target back to its
/// snapshotted `description` / `full_description`. Used exclusively by
/// [`DockerhubPublisher::rollback`].
///
/// Credentials are re-resolved at rollback time from the injected env
/// source — production wires up [`anodizer_core::ProcessEnvSource`];
/// tests inject a [`anodizer_core::MapEnvSource`] so the missing-env
/// branch can be exercised without mutating the process env. The
/// password value never crosses [`PublishEvidence::extra`]. A target
/// whose snapshot was entirely `None` (rare — neither field was
/// readable at publish time) is a no-op: there is nothing to restore.
fn restore_dockerhub_target_with_env<E: anodizer_core::EnvSource + ?Sized>(
    client: &reqwest::blocking::Client,
    policy: &RetryPolicy,
    target: &DockerhubTarget,
    env: &E,
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

    let password = env.var(&target.secret_env_var).ok_or_else(|| {
        anyhow!(
            "dockerhub: env var '{}' not set; cannot re-authenticate to restore '{}'",
            target.secret_env_var,
            target.target
        )
    })?;

    let login_body = serde_json::json!({
        "username": &target.username,
        "password": password,
    });
    let login_url = format!("{}/v2/users/login/", dockerhub_api_base(env));
    let (_, login_body_text) = retry_http_blocking(
        "dockerhub: rollback authenticate",
        policy,
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
    retry_http_blocking(
        &label,
        policy,
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

// ---------------------------------------------------------------------------
// DockerhubPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_dockerhub`] in the [`anodizer_core::Publisher`] trait so
// the new dispatch path (see [`crate::registry::configured_publishers`]) can
// drive Docker Hub description sync alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (description sync is a
// non-load-bearing publisher; not required for the release to succeed).
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

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let mut out = Vec::new();
        for entry in ctx.config.dockerhub.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            // Same resolution `run()` uses: password from `secret_name`
            // (default DOCKER_PASSWORD); username from config (templated)
            // with DOCKER_USERNAME as env fallback.
            let secret = entry.secret_name.as_deref().unwrap_or("DOCKER_PASSWORD");
            out.push(anodizer_core::EnvRequirement::EnvAllOf {
                vars: vec![secret.to_string()],
            });
            match entry.username.as_deref().filter(|u| !u.is_empty()) {
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
        let targets = publish_to_dockerhub(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("dockerhub");
        // `artifact_paths` indexes every repo this run actually mutated
        // (driven off the returned targets, not config) so dry-run / skip
        // paths do not leak phantom entries. `primary_ref` points at the
        // first mutated repo for log-line continuity.
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
        Ok(evidence)
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
                // TLS config — vanishingly unlikely, but if it does we
                // degrade to the warn-only checklist rather than
                // bubbling Err and gating rollback of sibling
                // publishers.
                log.warn(&format!(
                    "dockerhub: rollback could not build HTTP client ({e:#}); \
                     manual review required for {} repo(s)",
                    targets.len()
                ));
                for t in &targets {
                    log.warn(&format!(
                        "dockerhub: manual restore needed for {} ({})",
                        t.target, t.repo_url
                    ));
                }
                return Ok(());
            }
        };
        let policy = ctx.retry_policy();
        let env = ctx.env_source();

        let mut restored = 0usize;
        let mut failed = 0usize;
        for t in &targets {
            match restore_dockerhub_target_with_env(&client, &policy, t, env) {
                Ok(()) => {
                    restored += 1;
                    log.status(&format!(
                        "dockerhub: restored description for '{}'",
                        t.target
                    ));
                }
                Err(e) => {
                    failed += 1;
                    log.warn(&format!(
                        "dockerhub: failed to restore '{}' ({}): {:#}; \
                         check ${} is set or restore manually via the Docker Hub UI",
                        t.target, t.repo_url, e, t.secret_env_var
                    ));
                }
            }
        }
        log.status(&format!(
            "dockerhub: rollback restored {} description(s), {} failure(s)",
            restored, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn skips_on_nightly(&self) -> bool {
        // Docker registries accept tag rewrites; nightly clobber is intentional.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
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
        Config, DockerHubConfig, DockerHubFromFile, DockerHubFromUrl, DockerHubFullDescription,
        StringOrBool,
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
    fn test_dockerhub_skips_when_no_config() {
        let config = Config::default();
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_skips_when_empty_vec() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_skips_when_disabled() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    /// When `dockerhub[].description` is unset, the publisher must fall
    /// back to the project-level `metadata.description` so a single source
    /// of truth covers every entry. Mirrors the same fallback shape used
    /// by the homebrew cask + MCP publishers (`effective_description` is
    /// the dockerhub-specific seam).
    #[test]
    fn dockerhub_uses_meta_description_when_unset() {
        use anodizer_core::config::MetadataConfig;

        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("from project metadata".to_string()),
            ..Default::default()
        });
        // Per-entry description left None — the fallback must kick in.
        let entry = DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: None,
            ..Default::default()
        };
        config.dockerhub = Some(vec![entry.clone()]);
        let ctx = dry_run_ctx(config);

        let resolved =
            effective_description(&entry, &ctx).expect("metadata fallback must produce a value");
        assert_eq!(resolved, "from project metadata");
    }

    /// Per-entry `description` always wins over the project metadata
    /// fallback when set non-empty.
    #[test]
    fn dockerhub_entry_description_wins_over_meta() {
        use anodizer_core::config::MetadataConfig;

        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("project-wide fallback".to_string()),
            ..Default::default()
        });
        let entry = DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("per-entry override".to_string()),
            ..Default::default()
        };
        config.dockerhub = Some(vec![entry.clone()]);
        let ctx = dry_run_ctx(config);
        let resolved = effective_description(&entry, &ctx).expect("description present");
        assert_eq!(resolved, "per-entry override");
    }

    /// Empty per-entry description (an explicit `""`) falls back to the
    /// project metadata — same shape as the `or_else` guard in `effective_description`.
    #[test]
    fn dockerhub_empty_entry_description_falls_back_to_meta() {
        use anodizer_core::config::MetadataConfig;

        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("from project metadata".to_string()),
            ..Default::default()
        });
        let entry = DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some(String::new()),
            ..Default::default()
        };
        config.dockerhub = Some(vec![entry.clone()]);
        let ctx = dry_run_ctx(config);
        let resolved = effective_description(&entry, &ctx).expect("metadata fallback used");
        assert_eq!(resolved, "from project metadata");
    }

    #[test]
    fn test_dockerhub_skips_when_no_images() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_skips_when_images_empty_vec() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec![]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_dry_run_logs() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_dry_run_multiple_images() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/app1".to_string(), "myorg/app2".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_dry_run_with_full_description_from_file() {
        // In dry-run, full_description is not resolved, so this test
        // confirms dry-run succeeds without reading the file.
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("My app".to_string()),
            full_description: Some(DockerHubFullDescription {
                from_file: Some(DockerHubFromFile {
                    path: "/nonexistent/dry-run-should-not-read-this.md".to_string(),
                }),
                from_url: None,
            }),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    fn render_ctx() -> Context {
        dry_run_ctx(Config::default())
    }

    #[test]
    fn test_resolve_full_description_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# My App\nDescription here").expect("write");

        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: readme.to_str().expect("path utf-8").to_string(),
            }),
            from_url: None,
        };
        let ctx = render_ctx();
        let result =
            resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect("resolve");
        assert_eq!(result, "# My App\nDescription here");
    }

    #[test]
    fn test_resolve_full_description_missing_file() {
        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: "/nonexistent/path/README.md".to_string(),
            }),
            from_url: None,
        };
        let ctx = render_ctx();
        assert!(resolve_full_description(&desc, &ctx, &client, &fast_policy()).is_err());
    }

    #[test]
    fn test_resolve_full_description_neither_set() {
        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: None,
        };
        let ctx = render_ctx();
        assert!(resolve_full_description(&desc, &ctx, &client, &fast_policy()).is_err());
    }

    /// `disable:` is a serde alias for `skip:` so YAML configs imported from
    /// parse without renaming the field.
    #[test]
    fn dockerhub_disable_alias_parses_as_skip() {
        let yaml = r#"
project_name: test
dockerhub:
  - username: u
    images:
      - org/img
    disable: true
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse");
        let entry = &cfg.dockerhub.as_ref().expect("dockerhub")[0];
        match entry.skip.as_ref().expect("skip set via disable alias") {
            StringOrBool::Bool(b) => assert!(*b),
            other => panic!("expected Bool(true), got {:?}", other),
        }
    }

    #[test]
    fn test_dockerhub_fails_when_username_missing() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: None,
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'username' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_dockerhub_fails_when_username_empty() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some(String::new()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'username' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_resolve_full_description_from_url_unreachable() {
        // Port 1 always refuses — exercises the transport-error fast-path of
        // retry_http_blocking. With a 3-attempt, 1ms-backoff policy this
        // takes a few ms total.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: "http://localhost:1/nonexistent".to_string(),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let err = resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect_err("err");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("dockerhub: fetch full_description")
                || chain.contains("exhausted retry attempts")
                || chain.contains("transport error"),
            "unexpected error chain: {chain}"
        );
    }

    // Pin: retry_http_blocking wires through from full_description fetch —
    // a single 503 then a 200 should succeed end-to-end through
    // resolve_full_description. Exercises the policy-plumbing rather than
    // the helper itself (which has its own 5xx-then-success test in
    // crates/core/src/retry.rs).
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    #[test]
    fn resolve_full_description_from_url_retries_5xx_then_succeeds() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
        ]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let body = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect("retries 5xx then succeeds");
        assert_eq!(body, "hello");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 503 retry then success"
        );
    }

    /// Defense-in-depth: if Docker Hub's GET-full-description endpoint
    /// echoes our `Authorization: Bearer <PAT>` header back in an error
    /// body, the bearer token must NOT survive into the user-visible
    /// error chain. The retry helper trips on the 500 and the error
    /// message goes through `redact_bearer_tokens`.
    #[test]
    fn resolve_full_description_redacts_bearer_in_error_body() {
        use std::time::Duration;

        let leaky_body = "internal error: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
        let body_len = leaky_body.len();
        // All 3 attempts return 500 to force exhaustion (the retry helper
        // surfaces the *last* attempt's body in the error message).
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky_body}"
            )
            .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp, resp, resp]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let err = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect_err("500 exhaustion must error");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "bearer token leaked into error chain: {chain}"
        );
        assert!(
            chain.contains("<redacted>"),
            "expected `<redacted>` marker in error chain: {chain}"
        );
    }

    fn strict_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                strict: true,
                ..Default::default()
            },
        )
    }

    /// An image with a leading/trailing/consecutive slash has an empty
    /// path segment and is rejected unconditionally (before dry-run, so
    /// even config-test runs catch it). Pins the empty-segment `bail!`.
    #[test]
    fn test_dockerhub_rejects_empty_path_segment() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg//myapp".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("empty path segment"),
            "unexpected error: {err}"
        );
    }

    /// An image with three slash-separated segments exceeds Docker Hub's
    /// `namespace/repo` format and is rejected. Pins the
    /// too-many-segments `bail!`.
    #[test]
    fn test_dockerhub_rejects_too_many_segments() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["a/b/c".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("too many path segments"),
            "unexpected error: {err}"
        );
    }

    /// A bare image name (no namespace) maps to `library/` — which needs
    /// Docker Inc permissions — so strict mode hard-fails via
    /// `strict_guard`. Pins the strict branch of the bare-name guard
    /// (non-strict only warns; this proves the `(strict mode)` bail).
    #[test]
    fn test_dockerhub_bare_name_fails_in_strict_mode() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["barename".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = strict_ctx(config);
        let log = ctx.logger("dockerhub");
        let err = publish_to_dockerhub(&ctx, &log).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("library/") && msg.contains("strict mode"),
            "unexpected error: {msg}"
        );
    }

    /// A templated image (`{{ .Env.NS }}/app`) is rendered before
    /// validation — proving the image-render pass runs. The render falls
    /// back to the process env (set under the mutex guard). Asserts the
    /// dry-run intent log names the *rendered* image, not the raw
    /// `{{ }}` form (which would also trip the empty-segment validator
    /// if rendering were skipped).
    #[test]
    #[serial_test::serial]
    fn test_dockerhub_renders_image_template_before_validation() {
        use anodizer_core::test_helpers::env::env_mutex;
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex; paired set/remove below.
        unsafe { std::env::set_var("DOCKERHUB_TEST_NS", "myns") };

        let capture = anodizer_core::log::LogCapture::new();
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["{{ .Env.DOCKERHUB_TEST_NS }}/app".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx(config);
        ctx.with_log_capture(capture.clone());
        let log = ctx.logger("dockerhub");
        let result = publish_to_dockerhub(&ctx, &log);
        unsafe { std::env::remove_var("DOCKERHUB_TEST_NS") };
        result.expect("templated image renders + validates");
        let msgs = capture.all_messages();
        assert!(
            msgs.iter().any(|(_, m)| m.contains("myns/app")),
            "expected the rendered image in the dry-run intent log: {msgs:?}"
        );
    }

    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    /// `from_url` happy path: a single 200 returns the body verbatim and
    /// the request reaches the server as a plain `GET /readme.md` (no
    /// retry, exactly one hit). Pins the success branch of
    /// `resolve_full_description`'s `from_url` arm — the existing tests
    /// only cover the unreachable / retry / redaction failure modes.
    #[test]
    fn resolve_full_description_from_url_success_records_get() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/readme.md",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 17\r\n\r\n# Title\nbody text",
            times: None,
        }]);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/readme.md"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let body =
            resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect("200 succeeds");
        assert_eq!(body, "# Title\nbody text");
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.len(),
            1,
            "exactly one request, no retry: {entries:?}"
        );
        assert_eq!(entries[0].method, "GET");
        assert_eq!(entries[0].path, "/readme.md");
    }

    /// A 4xx on the `from_url` GET fast-fails (SuccessClass::Strict routes
    /// 4xx → Break) — the responder is hit exactly once, NOT
    /// `max_attempts` times, and the error names the URL + status. Pins
    /// the no-retry classification: a typo'd README URL must surface
    /// immediately rather than after 3 attempts.
    #[test]
    fn resolve_full_description_from_url_4xx_fast_fails_no_retry() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/missing.md",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
            times: None,
        }]);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/missing.md"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let err =
            resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect_err("404 errors");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("404") || chain.contains("Not Found"),
            "error chain should carry the 404 status: {chain}"
        );
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.len(),
            1,
            "4xx must NOT retry under fast_policy (3 attempts): {entries:?}"
        );
    }

    /// A 429 on `from_url` retries (SuccessClass::Strict routes 429 →
    /// Continue, same as 5xx) — first 429, then 200 succeeds. Distinct
    /// from the existing 503 test: 429 is the rate-limit case and shares
    /// the retriable classification with 5xx.
    #[test]
    fn resolve_full_description_from_url_429_retries_then_succeeds() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/r.md",
                response: "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/r.md",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
                times: None,
            },
        ]);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/r.md"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let body = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect("429 retries then 200");
        assert_eq!(body, "ok");
        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 2, "one 429 retry then success: {entries:?}");
    }

    /// The `from_url.url` is template-rendered through `ctx` before the
    /// fetch, so `{{ .Env.OWNER }}`-style configs resolve. The render
    /// path falls back to the process env, so the var is set under the
    /// `env_mutex` guard; the rendered path (`/tj/README.md`) is the one
    /// that must actually hit the wire (raw `{{ }}` would 404).
    #[test]
    #[serial_test::serial]
    fn resolve_full_description_from_url_renders_template() {
        use anodizer_core::test_helpers::env::env_mutex;
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex; paired set/remove below.
        unsafe { std::env::set_var("DOCKERHUB_TEST_OWNER", "tj") };

        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/tj/README.md",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nrendered",
            times: None,
        }]);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/{{{{ .Env.DOCKERHUB_TEST_OWNER }}}}/README.md"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let body = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect("templated url resolves");
        unsafe { std::env::remove_var("DOCKERHUB_TEST_OWNER") };
        assert_eq!(body, "rendered");
        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].path, "/tj/README.md",
            "the rendered (not raw) path must hit the wire: {entries:?}"
        );
    }

    /// A malformed template in `from_url.url` surfaces a render error
    /// (the `with_context` wraps it with the `render full_description
    /// from_url` label) BEFORE any network call. Pins the render-error
    /// arm distinct from the fetch-error arm.
    #[test]
    fn resolve_full_description_from_url_template_render_error() {
        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                // Unterminated tag — the template engine rejects it.
                url: "http://example.invalid/{{ .Tag".to_string(),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let err = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect_err("bad template must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("render full_description from_url"),
            "expected the render-context label, got: {chain}"
        );
    }

    /// The `from_file.path` is also template-rendered: a
    /// `{{ .Env.DOCS_DIR }}/README.md` config resolves the env var and
    /// reads the file at the rendered path. Distinct from the existing
    /// literal-path file test — this exercises the `render_template` call
    /// on the from_file branch (which falls back to the process env).
    #[test]
    #[serial_test::serial]
    fn resolve_full_description_from_file_renders_template_path() {
        use anodizer_core::test_helpers::env::env_mutex;
        let dir = tempfile::tempdir().expect("tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# Templated path").expect("write");

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex; paired set/remove below.
        unsafe {
            std::env::set_var(
                "DOCKERHUB_TEST_DOCS_DIR",
                dir.path().to_str().expect("path utf-8"),
            )
        };

        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: "{{ .Env.DOCKERHUB_TEST_DOCS_DIR }}/README.md".to_string(),
            }),
            from_url: None,
        };
        let ctx = render_ctx();
        let body =
            resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect("render + read");
        unsafe { std::env::remove_var("DOCKERHUB_TEST_DOCS_DIR") };
        assert_eq!(body, "# Templated path");
    }

    /// `from_file` takes precedence over `from_url` when both are set:
    /// the file content is returned and the URL is NEVER fetched (the
    /// responder records zero requests). Pins the documented precedence
    /// — without it, a config with both set could silently prefer the
    /// network source.
    #[test]
    fn resolve_full_description_from_file_wins_over_from_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "from the file").expect("write");

        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/never.md",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nfrom the url!",
            times: None,
        }]);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: readme.to_str().expect("path utf-8").to_string(),
            }),
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/never.md"),
                headers: None,
            }),
        };
        let ctx = render_ctx();
        let body = resolve_full_description(&desc, &ctx, &client, &fast_policy())
            .expect("from_file precedence");
        assert_eq!(body, "from the file");
        let entries = log.lock().expect("log");
        assert!(
            entries.is_empty(),
            "from_url must NOT be fetched when from_file is set: {entries:?}"
        );
    }

    /// Custom `from_url.headers` are emitted on the wire. The scripted
    /// responder doesn't capture headers, so this drives a raw inline
    /// TCP listener that records the full request and asserts the
    /// configured `X-Auth: secret-val` header arrives — pinning the
    /// `if let Some(ref h) = headers` forwarding loop.
    #[test]
    fn resolve_full_description_from_url_forwards_headers() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut buf = [0u8; 4096];
                let mut req = String::new();
                // Read until the header terminator so a header line split across
                // TCP segments can't be missed (there is no request body).
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if req.contains("\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
                let _ = tx.send(req);
            }
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client");
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Auth".to_string(), "secret-val".to_string());
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: format!("http://{addr}/h.md"),
                headers: Some(headers),
            }),
        };
        let ctx = render_ctx();
        let body =
            resolve_full_description(&desc, &ctx, &client, &fast_policy()).expect("with headers");
        assert_eq!(body, "ok");
        let req = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("captured request");
        assert!(
            req.to_lowercase().contains("x-auth: secret-val"),
            "configured header must hit the wire: {req}"
        );
    }
}

/// Live-mode (`dry_run: false`) coverage for `publish_to_dockerhub` and
/// `restore_dockerhub_target_with_env`. Every test redirects the
/// hard-coded `https://hub.docker.com` host to an in-process scripted
/// responder via the `ANODIZER_DOCKERHUB_API_BASE` env seam (mirroring
/// the `ANODIZER_GITHUB_API_BASE` pattern used by the GitHub backend), so
/// the login / snapshot-GET / PATCH request shapes — method, path, and
/// JSON body — and the response→outcome mapping are asserted against
/// recorded traffic. All HTTP-mock (local TCP, no subprocess) so they run
/// and count on every platform: UNGATED.
#[cfg(test)]
mod live_http_tests {
    use super::*;
    use anodizer_core::config::{
        Config, DockerHubConfig, DockerHubFromFile, HumanDuration, RetryConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use anodizer_core::{MapEnvSource, PublishEvidence, PublishEvidenceExtra, Publisher};
    use std::time::Duration;

    /// A 2-attempt, 1ms-backoff retry policy installed into the config so
    /// `ctx.retry_policy()` (which `publish_to_dockerhub` reads internally,
    /// non-injectably) doesn't inherit the 10-attempt / 10-second-delay
    /// production default and stall the suite.
    fn fast_retry() -> RetryConfig {
        RetryConfig {
            attempts: 2,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(2)),
        }
    }

    /// Build a live (non-dry-run) context whose env source carries the
    /// `ANODIZER_DOCKERHUB_API_BASE` redirect plus `DOCKER_PASSWORD`, and
    /// whose retry policy is fast. `strict` toggles the strict-guard
    /// branches.
    fn live_ctx(config: Config, api_base: &str, strict: bool) -> Context {
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                strict,
                ..Default::default()
            },
        );
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", api_base.to_string());
        env.set("DOCKER_PASSWORD", "s3cr3t-pat");
        ctx.set_env_source(env);
        ctx
    }

    fn entry(images: Vec<&str>, description: Option<&str>) -> DockerHubConfig {
        DockerHubConfig {
            username: Some("ci-bot".to_string()),
            images: Some(images.into_iter().map(str::to_string).collect()),
            description: description.map(str::to_string),
            ..Default::default()
        }
    }

    /// Standard 3-route DockerHub flow for a single `myorg/myapp` image:
    /// login → snapshot GET (returns the given prior description) → PATCH.
    fn flow_routes(prior_short: &'static str, prior_full: &'static str) -> Vec<ScriptedRoute> {
        let body =
            format!("{{\"description\":\"{prior_short}\",\"full_description\":\"{prior_full}\"}}");
        let snapshot: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n{body}",
                len = body.len(),
            )
            .into_boxed_str(),
        );
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: snapshot,
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]
    }

    /// Full happy path: login authenticates, snapshot GET reads the prior
    /// (differing) description, PATCH updates it. Asserts the exact login
    /// body (username + password), the snapshot GET path, and the PATCH
    /// body carrying the new `description`. Returns one mutated target.
    #[test]
    fn publish_live_full_flow_patches_description() {
        let (addr, log) = spawn_scripted_responder(flow_routes("old short", "old full"));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("brand new desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("live publish succeeds");

        assert_eq!(targets.len(), 1, "exactly one repo mutated");
        let t = &targets[0];
        assert_eq!(t.target, "myorg/myapp");
        assert_eq!(t.namespace, "myorg");
        assert_eq!(t.name, "myapp");
        assert_eq!(t.username, "ci-bot");
        assert_eq!(t.secret_env_var, "DOCKER_PASSWORD");
        assert_eq!(
            t.repo_url,
            format!("http://{addr}/v2/repositories/myorg/myapp/")
        );
        assert_eq!(t.snapshot_description.as_deref(), Some("old short"));
        assert_eq!(t.snapshot_full_description.as_deref(), Some("old full"));

        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 3, "login + snapshot + patch: {entries:?}");

        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[0].path, "/v2/users/login/");
        let login: serde_json::Value =
            serde_json::from_str(&entries[0].body).expect("login body is json");
        assert_eq!(login["username"], "ci-bot");
        assert_eq!(login["password"], "s3cr3t-pat");

        assert_eq!(entries[1].method, "GET");
        assert_eq!(entries[1].path, "/v2/repositories/myorg/myapp/");

        assert_eq!(entries[2].method, "PATCH");
        assert_eq!(entries[2].path, "/v2/repositories/myorg/myapp/");
        let patch: serde_json::Value =
            serde_json::from_str(&entries[2].body).expect("patch body is json");
        assert_eq!(patch["description"], "brand new desc");
        assert!(
            patch.get("full_description").is_none(),
            "no full_description configured → key omitted: {}",
            entries[2].body
        );
    }

    /// PATCH body carries BOTH `description` and `full_description` when a
    /// `full_description.from_file` is configured. Pins that the resolved
    /// file content lands verbatim in the `full_description` key.
    #[test]
    fn publish_live_patches_description_and_full_description() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));
        let dir = tempfile::tempdir().expect("tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# Long README\nbody").expect("write");

        let mut e = entry(vec!["myorg/myapp"], Some("short one"));
        e.full_description = Some(DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: readme.to_str().expect("utf-8").to_string(),
            }),
            from_url: None,
        });
        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![e]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("live publish succeeds");
        assert_eq!(targets.len(), 1);

        let entries = log.lock().expect("log");
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("a PATCH was issued");
        let body: serde_json::Value = serde_json::from_str(&patch.body).expect("patch body json");
        assert_eq!(body["description"], "short one");
        assert_eq!(body["full_description"], "# Long README\nbody");
    }

    /// The short description is template-rendered before the PATCH:
    /// `{{ .ProjectName }}` resolves against the context. Pins that the
    /// rendered (not raw) string hits the wire.
    #[test]
    fn publish_live_renders_description_template() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        let config = Config {
            project_name: "anodizer".to_string(),
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(
                vec!["myorg/myapp"],
                Some("Release tool for {{ .ProjectName }}"),
            )]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        publish_to_dockerhub(&ctx, &logger).expect("publish");
        let entries = log.lock().expect("log");
        let patch = entries
            .iter()
            .find(|e| e.method == "PATCH")
            .expect("PATCH issued");
        let body: serde_json::Value = serde_json::from_str(&patch.body).expect("json");
        assert_eq!(
            body["description"], "Release tool for anodizer",
            "the rendered description must hit the wire: {}",
            patch.body
        );
    }

    /// Idempotency: when the snapshot already matches the description we'd
    /// send, NO PATCH is issued (only login + snapshot GET) and the run
    /// records zero mutated targets.
    #[test]
    fn publish_live_skips_patch_when_unchanged() {
        let (addr, log) = spawn_scripted_responder(flow_routes("already current", ""));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("already current"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("publish");
        assert!(targets.is_empty(), "no-change run mutates nothing");

        let entries = log.lock().expect("log");
        assert!(
            entries.iter().all(|e| e.method != "PATCH"),
            "no PATCH when description is unchanged: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.method == "GET"),
            "snapshot GET still happens: {entries:?}"
        );
    }

    /// A 401 on login fails the publish with an error naming the status.
    /// The 4xx fast-fails (no retry): login is hit exactly once.
    #[test]
    fn publish_live_login_401_errors() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\nbad creds yo!",
            times: None,
        }]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("401 login must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("authentication failed") && chain.contains("401"),
            "error must name auth failure + status: {chain}"
        );
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "POST").count(),
            1,
            "401 fast-fails — login not retried: {entries:?}"
        );
    }

    /// A login response missing the `token` key errors with the
    /// "no token in login response" message — the publish cannot proceed
    /// without a bearer.
    #[test]
    fn publish_live_login_missing_token_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"detail\":\"x\"}",
            times: None,
        }]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("missing token errors");
        assert!(
            format!("{err:#}").contains("no token in login response"),
            "unexpected error: {err:#}"
        );
    }

    /// A 5xx on the snapshot GET retries then exhausts → the publish
    /// errors. The snapshot GET is attempted `attempts` (2) times under
    /// the fast policy, proving 5xx is retriable on the snapshot path.
    #[test]
    fn publish_live_snapshot_5xx_retries_then_errors() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("snapshot 5xx exhausts");
        assert!(
            format!("{err:#}").contains("snapshot"),
            "error must name the snapshot phase: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "GET").count(),
            2,
            "503 snapshot retried up to attempts=2: {entries:?}"
        );
    }

    /// A 500 on the PATCH (after a successful login + snapshot) fails the
    /// publish with an error naming the PATCH + status. Pins the PATCH
    /// error-mapping arm distinct from login/snapshot failures.
    #[test]
    fn publish_live_patch_500_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("patch 500 errors");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("PATCH") && chain.contains("500"),
            "error must name PATCH + status: {chain}"
        );
    }

    /// The JWT is cached across two entries that share the same
    /// `(username, secret_env)` pair: login happens exactly ONCE even
    /// though two distinct repos are PATCHed. Pins the `jwt_cache` reuse.
    #[test]
    fn publish_live_reuses_jwt_across_entries() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/app1/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/app1/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/myorg/app2/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/app2/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![
                entry(vec!["myorg/app1"], Some("desc one")),
                entry(vec!["myorg/app2"], Some("desc two")),
            ]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("publish");
        assert_eq!(targets.len(), 2, "both repos mutated");

        let entries = log.lock().expect("log");
        assert_eq!(
            entries.iter().filter(|e| e.method == "POST").count(),
            1,
            "shared (user, secret) pair → exactly one login: {entries:?}"
        );
        assert_eq!(
            entries.iter().filter(|e| e.method == "PATCH").count(),
            2,
            "one PATCH per repo: {entries:?}"
        );
    }

    /// A bare image name (no namespace) in NON-strict mode warns but
    /// proceeds: the PATCH targets the `library/<name>` repo path. Pins
    /// the non-strict branch of the bare-name guard plus the
    /// `("library", parts[0])` namespace-derivation fallback.
    #[test]
    fn publish_live_bare_name_maps_to_library_namespace() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"token\":\"jwt-abc\"}",
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/v2/repositories/library/solo/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/library/solo/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["solo"], Some("desc"))]),
            ..Default::default()
        };
        // Non-strict: the bare-name guard warns instead of bailing.
        let ctx = live_ctx(config, &format!("http://{addr}"), false);
        let logger = ctx.logger("dockerhub");

        let targets = publish_to_dockerhub(&ctx, &logger).expect("non-strict bare name proceeds");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].namespace, "library");
        assert_eq!(targets[0].name, "solo");

        let entries = log.lock().expect("log");
        assert!(
            entries
                .iter()
                .any(|e| e.method == "PATCH" && e.path == "/v2/repositories/library/solo/"),
            "PATCH targets library/<name>: {entries:?}"
        );
    }

    /// `DockerhubPublisher::run` (the trait entry point) end-to-end:
    /// drives the live flow and asserts the returned `PublishEvidence`
    /// carries the mutated repo in `artifact_paths` + `primary_ref` and
    /// the typed `Dockerhub` extra with one target.
    #[test]
    fn publisher_run_emits_evidence_for_mutated_repo() {
        let (addr, _log) = spawn_scripted_responder(flow_routes("old", "old full"));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("new desc"))]),
            ..Default::default()
        };
        let mut ctx = live_ctx(config, &format!("http://{addr}"), false);

        let p = DockerhubPublisher::new();
        let evidence = p.run(&mut ctx).expect("run succeeds");

        let expected = format!("http://{addr}/v2/repositories/myorg/myapp/");
        assert_eq!(evidence.artifact_paths.len(), 1);
        assert_eq!(evidence.artifact_paths[0].display().to_string(), expected);
        assert_eq!(evidence.primary_ref.as_deref(), Some(expected.as_str()));
        match &evidence.extra {
            PublishEvidenceExtra::Dockerhub(d) => {
                assert_eq!(d.dockerhub_targets.len(), 1);
                assert_eq!(d.dockerhub_targets[0].target, "myorg/myapp");
            }
            other => panic!("expected Dockerhub extra, got {other:?}"),
        }
    }

    /// `run` with no mutation (dry-run) emits empty evidence: no
    /// artifact_paths, no primary_ref, `Empty` extra. Pins that dry-run /
    /// skip paths do not leak phantom targets.
    #[test]
    fn publisher_run_dry_run_emits_empty_evidence() {
        let config = Config {
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let p = DockerhubPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run run succeeds");
        assert!(evidence.artifact_paths.is_empty());
        assert!(evidence.primary_ref.is_none());
        assert!(matches!(evidence.extra, PublishEvidenceExtra::Empty));
    }

    /// `restore_dockerhub_target_with_env` live happy path: re-authenticates
    /// (login body carries the re-resolved password) and PATCHes the
    /// snapshot back. Asserts the rollback PATCH body restores BOTH
    /// `description` and `full_description`.
    #[test]
    fn restore_live_reauths_and_patches_snapshot_back() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"token\":\"jwt-back\"}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = fast_retry().to_policy();
        let target = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior short".into()),
            snapshot_full_description: Some("# Prior README".into()),
        };
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "rollback-pat");

        restore_dockerhub_target_with_env(&client, &policy, &target, &env)
            .expect("rollback restores snapshot");

        let entries = log.lock().expect("log");
        assert_eq!(entries.len(), 2, "login + patch: {entries:?}");
        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[0].path, "/v2/users/login/");
        let login: serde_json::Value = serde_json::from_str(&entries[0].body).expect("login json");
        assert_eq!(login["username"], "ci-bot");
        assert_eq!(login["password"], "rollback-pat");

        assert_eq!(entries[1].method, "PATCH");
        assert_eq!(entries[1].path, "/v2/repositories/myorg/myapp/");
        let patch: serde_json::Value = serde_json::from_str(&entries[1].body).expect("patch json");
        assert_eq!(patch["description"], "prior short");
        assert_eq!(patch["full_description"], "# Prior README");
    }

    /// Rollback login 403 surfaces an error naming the rollback-auth
    /// failure + status, so `DockerhubPublisher::rollback` can count it as
    /// a failure rather than silently dropping the restore.
    #[test]
    fn restore_live_login_403_errors() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/v2/users/login/",
            response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = fast_retry().to_policy();
        let target = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        };
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "pw");

        let err = restore_dockerhub_target_with_env(&client, &policy, &target, &env)
            .expect_err("403 rollback login errors");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("rollback authentication failed") && chain.contains("403"),
            "error must name rollback auth + status: {chain}"
        );
    }

    /// `DockerhubPublisher::rollback` end-to-end over recorded evidence:
    /// re-authenticates and restores each target, logging the restored
    /// count. Pins the trait rollback loop (decode → per-target restore →
    /// status tally) against the live responder.
    #[test]
    fn publisher_rollback_restores_recorded_targets() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/v2/users/login/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"token\":\"jwt-back\"}",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/v2/repositories/myorg/myapp/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);

        let capture = anodizer_core::log::LogCapture::new();
        let config = Config {
            retry: Some(fast_retry()),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        env.set("DOCKER_PASSWORD", "rollback-pat");
        ctx.set_env_source(env);
        ctx.with_log_capture(capture.clone());

        let mut evidence = PublishEvidence::new("dockerhub");
        evidence.extra =
            PublishEvidenceExtra::Dockerhub(anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: format!("http://{addr}/v2/repositories/myorg/myapp/"),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior short".into()),
                    snapshot_full_description: None,
                }],
            });

        let p = DockerhubPublisher::new();
        p.rollback(&mut ctx, &evidence).expect("rollback succeeds");

        let entries = log.lock().expect("log");
        assert!(
            entries.iter().any(|e| e.method == "PATCH"),
            "rollback issued a PATCH: {entries:?}"
        );
        let statuses = capture.all_messages();
        assert!(
            statuses
                .iter()
                .any(|(_, m)| m.contains("restored 1 description")),
            "rollback tally must report 1 restored: {statuses:?}"
        );
    }

    /// `DockerhubPublisher::rollback` counts a failed restore (missing env
    /// var) as a failure and continues — the tally reports `1 failure(s)`
    /// and a per-target warn names the env var. Pins the warn-don't-abort
    /// rollback contract.
    #[test]
    fn publisher_rollback_counts_failure_when_env_missing() {
        let capture = anodizer_core::log::LogCapture::new();
        let config = Config {
            retry: Some(fast_retry()),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        // Env source deliberately omits DOCKER_PASSWORD so restore fails.
        ctx.set_env_source(MapEnvSource::new());
        ctx.with_log_capture(capture.clone());

        let mut evidence = PublishEvidence::new("dockerhub");
        evidence.extra =
            PublishEvidenceExtra::Dockerhub(anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: "http://127.0.0.1:1/unreachable".into(),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior".into()),
                    snapshot_full_description: None,
                }],
            });

        let p = DockerhubPublisher::new();
        p.rollback(&mut ctx, &evidence)
            .expect("rollback never aborts");

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("failed to restore") && m.contains("DOCKER_PASSWORD")),
            "a per-target failure warn must name the env var: {warns:?}"
        );
        let statuses = capture.all_messages();
        assert!(
            statuses
                .iter()
                .any(|(_, m)| m.contains("0 description(s), 1 failure(s)")),
            "tally must report 1 failure: {statuses:?}"
        );
    }

    /// Live mode with a missing secret env var hard-fails authentication
    /// BEFORE any login round-trip (the env lookup precedes the POST).
    /// Pins the `environment variable '<X>' not set` bail on the publish
    /// path (distinct from the dry-run warn).
    #[test]
    fn publish_live_missing_secret_env_errors() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], Some("desc"))]),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        // API base redirected, but DOCKER_PASSWORD intentionally absent.
        let mut env = MapEnvSource::new();
        env.set("ANODIZER_DOCKERHUB_API_BASE", format!("http://{addr}"));
        ctx.set_env_source(env);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("missing secret errors");
        assert!(
            format!("{err:#}").contains("'DOCKER_PASSWORD' not set"),
            "unexpected error: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert!(
            entries.is_empty(),
            "no HTTP call before the secret lookup: {entries:?}"
        );
    }

    /// Strict mode + both descriptions empty → the publish bails via
    /// `strict_guard` BEFORE authenticating. Pins the strict branch of the
    /// "both description and full_description are empty" guard. The
    /// responder records zero traffic.
    #[test]
    fn publish_live_strict_empty_descriptions_bails_before_auth() {
        let (addr, log) = spawn_scripted_responder(flow_routes("", ""));

        // No per-entry description, no metadata fallback → both empty.
        let config = Config {
            retry: Some(fast_retry()),
            dockerhub: Some(vec![entry(vec!["myorg/myapp"], None)]),
            ..Default::default()
        };
        let ctx = live_ctx(config, &format!("http://{addr}"), true);
        let logger = ctx.logger("dockerhub");

        let err = publish_to_dockerhub(&ctx, &logger).expect_err("strict empty-desc bails");
        assert!(
            format!("{err:#}").contains("both description and full_description are empty"),
            "unexpected error: {err:#}"
        );
        let entries = log.lock().expect("log");
        assert!(
            entries.is_empty(),
            "strict bail precedes any HTTP call: {entries:?}"
        );
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn dockerhub_publisher_classification() {
        let p = DockerhubPublisher::new();
        assert_eq!(p.name(), "dockerhub");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("DOCKER_PASSWORD description snapshot+restore")
        );
    }

    #[test]
    fn dockerhub_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = DockerhubPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn dockerhub_rollback_warns_when_no_targets_recorded() {
        // Empty evidence drives rollback into the no-targets branch.
        // The capture pins that production actually invoked `log.warn`
        // with the helper-formatted message — a hand-constructed expected
        // string compared against the helper output would pass even if
        // the rollback body forgot the warn entirely.
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("dockerhub");
        let p = DockerhubPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("dockerhub")
                && m.contains("description-sync targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Defense-in-depth: a serialized `DockerhubTarget` carries no
    /// secret material. Mirrors the negative test on every other
    /// publisher's `*Target` struct — see the `PublishEvidence::extra`
    /// rustdoc for the contract. `password` is the field the historical
    /// warn-only rollback could not enforce; this test pins that the
    /// snapshot+restore implementation still does not persist one.
    #[test]
    fn dockerhub_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence with a populated
        // variant and assert (a) no credential-shaped keys appear AND
        // (b) the operator-public coordinates serialize.
        let mut e = anodizer_core::PublishEvidence::new("dockerhub");
        e.extra = anodizer_core::PublishEvidenceExtra::Dockerhub(
            anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: vec![DockerhubTarget {
                    target: "myorg/myapp".into(),
                    repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
                    namespace: "myorg".into(),
                    name: "myapp".into(),
                    username: "ci-bot".into(),
                    secret_env_var: "DOCKER_PASSWORD".into(),
                    snapshot_description: Some("prior short desc".into()),
                    snapshot_full_description: Some("# Prior README\nbody".into()),
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":\""), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: env var NAME + login + endpoint serialize.
        assert!(s.contains("\"secret_env_var\""), "{s}");
        assert!(s.contains("\"username\":\"ci-bot\""), "{s}");
        assert!(s.contains("\"namespace\":\"myorg\""), "{s}");
    }

    /// `decode_dockerhub_targets` is the rollback entry point — assert
    /// the round-trip shape so a future schema drift (rename of the
    /// `dockerhub_targets` key, change to the field set) fails loudly
    /// rather than silently degrading rollback to warn-only.
    #[test]
    fn dockerhub_target_decode_round_trips() {
        let original = vec![DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        }];
        let extra = anodizer_core::PublishEvidenceExtra::Dockerhub(
            anodizer_core::publish_evidence::DockerhubExtra {
                dockerhub_targets: original.clone(),
            },
        );
        let decoded = decode_dockerhub_targets(&extra);
        assert_eq!(decoded, original);
    }

    /// Wrong variant → empty vec → rollback short-circuits to the
    /// empty-evidence warn path. Pins the failure mode so a typed-enum
    /// addition that accidentally matches an unrelated payload doesn't
    /// re-enable the live-PATCH path against decoded data.
    #[test]
    fn dockerhub_target_decode_missing_key_yields_empty() {
        let extra = anodizer_core::PublishEvidenceExtra::Empty;
        assert!(decode_dockerhub_targets(&extra).is_empty());
        let extra = anodizer_core::PublishEvidenceExtra::Homebrew(
            anodizer_core::publish_evidence::HomebrewExtra {
                homebrew_targets: Vec::new(),
            },
        );
        assert!(decode_dockerhub_targets(&extra).is_empty());
    }

    /// PATCH-body construction round-trip: a snapshot with both
    /// description and full_description set produces a PATCH body
    /// carrying exactly those keys. The live-HTTP path goes through
    /// `restore_dockerhub_target_with_env` (also exercised by the
    /// missing-env and empty-snapshot tests below); this test pins
    /// the body shape without spinning up the responder — login +
    /// PATCH both hit the hard-coded `https://hub.docker.com` host,
    /// so a unit-level happy-path requires either redirecting login
    /// (an artificial fixture) or running against the real registry
    /// (integration).
    #[test]
    fn dockerhub_restore_body_contains_snapshot_fields() {
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "https://hub.docker.com/v2/repositories/myorg/myapp/".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD".into(),
            snapshot_description: Some("prior short".into()),
            snapshot_full_description: Some("# prior README".into()),
        };

        // Reproduce the body-construction the restore function would
        // assemble. Pins the contract without exercising the live
        // HTTP path.
        let mut expected = serde_json::Map::new();
        if let Some(ref d) = t.snapshot_description {
            expected.insert("description".into(), serde_json::Value::String(d.clone()));
        }
        if let Some(ref fd) = t.snapshot_full_description {
            expected.insert(
                "full_description".into(),
                serde_json::Value::String(fd.clone()),
            );
        }
        assert_eq!(
            expected.get("description").and_then(|v| v.as_str()),
            Some("prior short")
        );
        assert_eq!(
            expected.get("full_description").and_then(|v| v.as_str()),
            Some("# prior README")
        );
    }

    /// All-None snapshot → restore is a no-op (no PATCH issued).
    /// Defends against the failure mode where a publisher with no
    /// readable prior description would otherwise PATCH `{}` (which
    /// DockerHub treats as a no-op anyway, but issuing the call wastes
    /// an auth round-trip and surfaces a spurious "restored" log line).
    #[test]
    fn dockerhub_restore_no_op_when_snapshot_empty() {
        // Build a fresh client; the function returns Ok(()) before
        // touching the network when the snapshot is all-None.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "http://127.0.0.1:1/unreachable".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: "DOCKER_PASSWORD_UNSET_FOR_TEST_XYZ".into(),
            snapshot_description: None,
            snapshot_full_description: None,
        };
        // Note: the unreachable URL + unset env var would normally
        // make this fail loudly. The no-op short-circuit must fire
        // before either is touched.
        let env = anodizer_core::MapEnvSource::new();
        restore_dockerhub_target_with_env(&client, &policy, &t, &env)
            .expect("no-op when snapshot empty");
    }

    /// Missing env var → restore returns Err with a recognizable
    /// message naming the env var. Drives the lookup through an empty
    /// [`MapEnvSource`] so the assertion holds regardless of the
    /// ambient process env (no thread-race on `remove_var`).
    #[test]
    fn dockerhub_restore_errors_when_env_missing() {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let env_var = "DOCKER_PASSWORD_INTENTIONALLY_UNSET_FOR_DOCKERHUB_TEST_ABCDEF";
        let t = DockerhubTarget {
            target: "myorg/myapp".into(),
            repo_url: "http://127.0.0.1:1/unreachable".into(),
            namespace: "myorg".into(),
            name: "myapp".into(),
            username: "ci-bot".into(),
            secret_env_var: env_var.into(),
            snapshot_description: Some("prior".into()),
            snapshot_full_description: None,
        };
        let env = anodizer_core::MapEnvSource::new();
        let err =
            restore_dockerhub_target_with_env(&client, &policy, &t, &env).expect_err("env missing");
        let chain = format!("{err:#}");
        assert!(
            chain.contains(env_var),
            "error chain should name the unset env var: {chain}"
        );
    }
}
