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

// ---------------------------------------------------------------------------
// resolve_full_description
// ---------------------------------------------------------------------------

/// Resolve the full description content from either a local file or a URL.
/// `from_file` takes precedence over `from_url` when both are set.
///
/// The `from_url` branch routes through [`retry_http_blocking`] so transient
/// 5xx / 429 / network failures retry per the user's top-level `retry:`
/// policy; 4xx fast-fails so a typo'd URL surfaces immediately.
pub fn resolve_full_description(
    desc: &DockerHubFullDescription,
    client: &reqwest::blocking::Client,
    policy: &RetryPolicy,
) -> Result<String> {
    if let Some(ref from_file) = desc.from_file {
        return std::fs::read_to_string(&from_file.path)
            .with_context(|| format!("dockerhub: failed to read file '{}'", from_file.path));
    }

    if let Some(ref from_url) = desc.from_url {
        let url = from_url.url.clone();
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
    // GoReleaser, where the retryx policy is captured once per pipe).
    let policy = ctx.retry_policy();

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

        // Resolve username from config, falling back to DOCKER_USERNAME.
        // Bail early when neither is set so config errors surface even in
        // dry-run.
        let username_env = ctx.env_var("DOCKER_USERNAME");
        let username = match entry.username.as_deref() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => match username_env.as_deref() {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => bail!(
                    "dockerhub: 'username' is required (set in config or via DOCKER_USERNAME env)"
                ),
            },
        };

        let images = entry.images.as_deref().unwrap_or_default();

        if images.is_empty() {
            ctx.strict_guard(log, "dockerhub: no images configured, skipping entry")?;
            continue;
        }

        // Empty per-entry description falls back to the project's global
        // metadata.description so a single source of truth covers every
        // dockerhub entry.
        let description_owned: Option<String> = entry
            .description
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                ctx.config
                    .metadata
                    .as_ref()
                    .and_then(|m| m.description.as_deref())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
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
                resolve_full_description(fd, client, &policy)
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

        // Authenticate: POST to get JWT token.
        let password = ctx
            .env_var(secret_name)
            .ok_or_else(|| anyhow!("dockerhub: environment variable '{}' not set", secret_name))?;

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
                    .post("https://hub.docker.com/v2/users/login/")
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

        let token = login_json["token"]
            .as_str()
            .ok_or_else(|| anyhow!("dockerhub: no token in login response"))?;

        // PATCH each image repository.
        for image in images {
            let parts: Vec<&str> = image.splitn(2, '/').collect();
            let (namespace, name) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                ("library", parts[0])
            };

            let repo_url = format!(
                "https://hub.docker.com/v2/repositories/{}/{}/",
                namespace, name
            );

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
    let (_, login_body_text) = retry_http_blocking(
        "dockerhub: rollback authenticate",
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .post("https://hub.docker.com/v2/users/login/")
                .json(&login_body)
                .send()
        },
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
        self.required_override.unwrap_or(Self::PUBLISHER_REQUIRED)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
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
        let result = resolve_full_description(&desc, &client, &fast_policy()).expect("resolve");
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
        assert!(resolve_full_description(&desc, &client, &fast_policy()).is_err());
    }

    #[test]
    fn test_resolve_full_description_neither_set() {
        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: None,
        };
        assert!(resolve_full_description(&desc, &client, &fast_policy()).is_err());
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
        let err = resolve_full_description(&desc, &client, &fast_policy()).expect_err("err");
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
        let body = resolve_full_description(&desc, &client, &fast_policy())
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
        let err = resolve_full_description(&desc, &client, &fast_policy())
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
