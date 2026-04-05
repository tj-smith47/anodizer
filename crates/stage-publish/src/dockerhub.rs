use anodize_core::config::DockerHubFullDescription;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{anyhow, bail, Context as _, Result};

// ---------------------------------------------------------------------------
// resolve_full_description
// ---------------------------------------------------------------------------

/// Resolve the full description content from either a local file or a URL.
/// `from_file` takes precedence over `from_url` when both are set.
pub fn resolve_full_description(
    desc: &DockerHubFullDescription,
    client: &reqwest::blocking::Client,
) -> Result<String> {
    if let Some(ref from_file) = desc.from_file {
        return std::fs::read_to_string(&from_file.path)
            .with_context(|| format!("dockerhub: failed to read file '{}'", from_file.path));
    }

    if let Some(ref from_url) = desc.from_url {
        let mut req = client.get(&from_url.url);
        if let Some(ref headers) = from_url.headers {
            for (key, value) in headers {
                req = req.header(key, value);
            }
        }
        let resp = req
            .send()
            .with_context(|| format!("dockerhub: failed to fetch URL '{}'", from_url.url))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            bail!(
                "dockerhub: GET {} returned HTTP {}: {}",
                from_url.url,
                status,
                body
            );
        }
        return resp
            .text()
            .with_context(|| format!("dockerhub: failed to read body from '{}'", from_url.url));
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

    for entry in entries {
        // Check disable flag.
        if let Some(ref d) = entry.disable {
            if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                log.status("dockerhub: entry disabled, skipping");
                continue;
            }
        }

        // Critical 1: Bail early when username is missing or empty (before
        // dry-run check so config errors surface even in dry-run mode).
        let username = match entry.username.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => {
                bail!("dockerhub: 'username' is required but not set");
            }
        };

        let images = entry
            .images
            .as_deref()
            .unwrap_or_default();

        if images.is_empty() {
            log.warn("dockerhub: no images configured, skipping entry");
            continue;
        }

        // Important 5: Validate short description length.
        let short_desc = entry.description.as_deref().unwrap_or("");
        if short_desc.len() > 100 {
            log.warn(&format!(
                "dockerhub: short description is {} chars (max 100); Docker Hub will truncate it",
                short_desc.len()
            ));
        }

        // Suggestion 8: Warn when bare image names map to library/ namespace.
        for image in images {
            if !image.contains('/') {
                log.warn(&format!(
                    "dockerhub: image '{}' has no namespace; bare names map to 'library/' which requires Docker Inc permissions",
                    image
                ));
            }
        }

        // Important 3: Dry-run check BEFORE resolving full description.
        if ctx.is_dry_run() {
            for image in images {
                log.status(&format!(
                    "(dry-run) would sync DockerHub description for '{}'",
                    image
                ));
            }
            continue;
        }

        // Important 4: Create a single client for the entire entry.
        let client = reqwest::blocking::Client::new();

        // Resolve full description if configured (after dry-run check).
        let full_desc = match entry.full_description {
            Some(ref fd) => Some(
                resolve_full_description(fd, &client)
                    .context("dockerhub: failed to resolve full_description")?,
            ),
            None => None,
        };

        // Critical 2: Skip PATCH when both descriptions are absent.
        if short_desc.is_empty() && full_desc.is_none() {
            log.warn("dockerhub: both description and full_description are empty, skipping PATCH");
            continue;
        }

        // Authenticate: POST to get JWT token.
        let secret_name = entry
            .secret_name
            .as_deref()
            .unwrap_or("DOCKER_PASSWORD");

        let password = std::env::var(secret_name).with_context(|| {
            format!(
                "dockerhub: environment variable '{}' not set",
                secret_name
            )
        })?;

        let login_body = serde_json::json!({
            "username": username,
            "password": password,
        });

        let login_resp = client
            .post("https://hub.docker.com/v2/users/login/")
            .json(&login_body)
            .send()
            .context("dockerhub: failed to authenticate with Docker Hub")?;

        // Important 6: Include response body in login error message.
        if !login_resp.status().is_success() {
            let status = login_resp.status();
            let body = login_resp.text().unwrap_or_default();
            bail!(
                "dockerhub: authentication failed (HTTP {}): {}",
                status,
                body
            );
        }

        let login_json: serde_json::Value = login_resp
            .json()
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

            let patch_resp = client
                .patch(&format!(
                    "https://hub.docker.com/v2/repositories/{}/{}/",
                    namespace, name
                ))
                .bearer_auth(token)
                .json(&patch_body)
                .send()
                .with_context(|| {
                    format!("dockerhub: failed to PATCH repository '{}'", image)
                })?;

            // Important 6: Include response body in PATCH error message.
            if !patch_resp.status().is_success() {
                let status = patch_resp.status();
                let body = patch_resp.text().unwrap_or_default();
                bail!(
                    "dockerhub: PATCH {}/{} failed (HTTP {}): {}",
                    namespace,
                    name,
                    status,
                    body
                );
            }

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
    use anodize_core::config::{
        Config, DockerHubConfig, DockerHubFromFile, DockerHubFromUrl, DockerHubFullDescription,
        StringOrBool,
    };
    use anodize_core::context::{Context, ContextOptions};

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
            disable: Some(StringOrBool::Bool(true)),
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

    // Finding 7: Test that empty images vec is also skipped.
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
            images: Some(vec![
                "myorg/app1".to_string(),
                "myorg/app2".to_string(),
            ]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("dockerhub");
        assert!(publish_to_dockerhub(&ctx, &log).is_ok());
    }

    #[test]
    fn test_dockerhub_dry_run_with_full_description_from_file() {
        // In dry-run mode, full_description is NOT resolved (Important 3),
        // so this test just confirms dry-run succeeds without reading the file.
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

    #[test]
    fn test_resolve_full_description_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# My App\nDescription here").unwrap();

        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile {
                path: readme.to_str().unwrap().to_string(),
            }),
            from_url: None,
        };
        let result = resolve_full_description(&desc, &client).unwrap();
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
        assert!(resolve_full_description(&desc, &client).is_err());
    }

    #[test]
    fn test_resolve_full_description_neither_set() {
        let client = reqwest::blocking::Client::new();
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: None,
        };
        assert!(resolve_full_description(&desc, &client).is_err());
    }

    // Finding 1: Username missing should bail.
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

    // Finding 1: Empty username string should also bail.
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

    // Finding 9: from_url with unreachable URL should error.
    #[test]
    fn test_resolve_full_description_from_url_unreachable() {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        let desc = DockerHubFullDescription {
            from_file: None,
            from_url: Some(DockerHubFromUrl {
                url: "http://localhost:1/nonexistent".to_string(),
                headers: None,
            }),
        };
        let err = resolve_full_description(&desc, &client).unwrap_err();
        assert!(
            err.to_string().contains("failed to fetch URL"),
            "unexpected error: {}",
            err
        );
    }
}
