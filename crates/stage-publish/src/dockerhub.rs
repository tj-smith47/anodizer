use anodizer_core::config::DockerHubFullDescription;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, anyhow, bail};

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
pub fn publish_to_dockerhub(ctx: &Context, log: &StageLogger) -> Result<()> {
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
        let username_env = std::env::var("DOCKER_USERNAME").ok();
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
        if ctx.is_dry_run() && std::env::var(secret_name).ok().is_none() {
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
        let password = std::env::var(secret_name).with_context(|| {
            format!("dockerhub: environment variable '{}' not set", secret_name)
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

            let patch_url = format!(
                "https://hub.docker.com/v2/repositories/{}/{}/",
                namespace, name
            );
            let label = format!("dockerhub: PATCH {}", image);
            retry_http_blocking(
                &label,
                &policy,
                SuccessClass::Strict,
                |_| {
                    client
                        .patch(&patch_url)
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
        }
    }

    Ok(())
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
    fn spawn_oneshot_http_responder(
        responses: Vec<&'static str>,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let counter = std::sync::Arc::new(AtomicU32::new(0));
        let counter_inner = counter.clone();
        std::thread::spawn(move || {
            for (i, resp) in responses.iter().enumerate() {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                counter_inner.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                if i == responses.len() - 1 {
                    break;
                }
            }
        });
        (addr, counter)
    }

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
