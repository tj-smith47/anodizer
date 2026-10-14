//! Pre-flight validation for the SchemaStore publisher.
//!
//! Reads every non-skipped `schemas` entry and self-checks what it can before
//! the publish stage runs: config shape, catalog description content rules,
//! and — for vendor mode — that the schema file exists, parses, and carries a
//! valid `$id`/dialect. The publisher's `preflight()` forwards here.

use anodizer_core::PreflightCheck;
use anodizer_core::config::{SchemaEntry, SchemaMode};
use anodizer_core::context::Context;

use super::entry_label;
use super::manifest;

/// Validate every non-skipped schema entry before the publish stage runs.
///
/// Per entry, in order: config-shape (`validate`), description content rules
/// (resolving the DERIVED description when none is set, so the omitted-
/// `description` path is validated here exactly as the publish stage will),
/// and — for vendor mode — that the schema file exists on disk, parses as
/// JSON, carries an http(s) `$id`, and uses a recognized json-schema dialect.
/// External entries are additionally checked for a well-formed http(s) `url`.
///
/// Aggregation is first-blocker-wins: the first [`PreflightCheck::Blocker`]
/// short-circuits and is returned; absent any blocker the first
/// [`PreflightCheck::Warning`] is returned; otherwise [`PreflightCheck::Pass`].
pub(crate) fn preflight_checks(ctx: &Context) -> anyhow::Result<PreflightCheck> {
    let cfg = &ctx.config.schemastore;
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let mut first_warning: Option<String> = None;

    for entry in &cfg.schemas {
        if cfg.resolved_skip(entry) {
            continue;
        }

        if let Err(e) = entry.validate() {
            return Ok(PreflightCheck::Blocker(format!(
                "{}: {e}",
                entry_label(&entry.name)
            )));
        }

        // Resolve+sanitize through the SAME path the publish stage uses, so a
        // DERIVED description (the omitted-`description` case) is validated here
        // too — not just an explicit one. A derived Cargo description containing
        // "schema" would otherwise pass preflight and fail mid-publish.
        if let Err(e) = super::publish::resolve_description(ctx, entry) {
            // `e` already carries the entry label; append a hint pointing at the
            // derived-description path so the operator knows to set an explicit
            // `description:` and can leave the project/crate metadata alone.
            let hint = if entry.description.is_none() {
                " — derived from project/crate metadata; set an explicit \
                 `description:` to override"
            } else {
                ""
            };
            return Ok(PreflightCheck::Blocker(format!("{e}{hint}")));
        }

        match entry.mode()? {
            SchemaMode::Vendor => {
                if let Some(check) = preflight_vendor(&root, entry, &mut first_warning) {
                    return Ok(check);
                }
            }
            SchemaMode::External => {
                preflight_external(entry, &mut first_warning);
            }
        }
    }

    if cfg.schemas.iter().any(|entry| !cfg.resolved_skip(entry)) {
        match token_checks(ctx) {
            PreflightCheck::Blocker(m) => return Ok(PreflightCheck::Blocker(m)),
            PreflightCheck::Warning(w) => {
                first_warning.get_or_insert(w);
            }
            PreflightCheck::Pass => {}
        }
    }

    if let Some(w) = first_warning {
        return Ok(PreflightCheck::Warning(w));
    }
    Ok(PreflightCheck::Pass)
}

/// Probe the fork with the token the publish pushes with
/// ([`crate::util::resolve_repo_token`]) and `SchemaStore/schemastore` with
/// the token the publish opens the pull request with
/// ([`crate::util::resolve_pr_token`]), both read through
/// `SCHEMASTORE_TOKEN` as the publish reads them. A token template that does
/// not render is a blocker naming its field. No pull request probe runs
/// when no pull request token resolves, since `gh` then opens it with its
/// own login. A block with no fork configured is left to the publish, which
/// names the missing `repository`.
fn token_checks(ctx: &Context) -> PreflightCheck {
    use super::publish::{TOKEN_ENV_VAR, UPSTREAM_OWNER, UPSTREAM_REPO};
    let Some(repo) = ctx.config.schemastore.repository.as_ref() else {
        return PreflightCheck::Pass;
    };
    if crate::util::resolve_repo_owner_name(Some(repo)).is_none() {
        return PreflightCheck::Pass;
    }
    let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
    // The push probe reads `repository.token` only; the pull request goes to
    // the fixed upstream below, so the fork probe is asked for no pull
    // request of its own.
    let fork = anodizer_core::config::RepositoryConfig {
        pull_request: None,
        ..repo.clone()
    };
    let push = crate::publisher_preflight::github_repo_config_check(
        ctx,
        Some(&fork),
        TOKEN_ENV_VAR,
        None,
        &policy,
    );
    let pr_token = match crate::util::resolve_pr_token(ctx, Some(repo), Some(TOKEN_ENV_VAR)) {
        Ok(Some(token)) => token,
        Ok(None) => return push,
        Err(e) => return push.merge(PreflightCheck::Blocker(format!("schemastore: {e:#}"))),
    };
    let probe = anodizer_core::git::GithubRepoProbe {
        owner: UPSTREAM_OWNER,
        repo: UPSTREAM_REPO,
        token: Some(&pr_token),
        policy: &policy,
        deadline: ctx.retry_deadline(),
        strict: ctx.preflight_is_strict(),
    };
    let base = anodizer_core::http::github_api_base(ctx.env_source());
    push.merge(
        crate::publisher_preflight::github_pull_request_repo_check_at(
            &format!("{base}/repos/{UPSTREAM_OWNER}/{UPSTREAM_REPO}"),
            &probe,
            &ctx.logger("preflight"),
        ),
    )
}

/// Vendor-entry preflight: read the schema file off disk, parse it, and check
/// `$schema`/`$id`. Returns `Some(Blocker)` to short-circuit the whole
/// preflight; returns `None` (recording at most a warning in `first_warning`)
/// to continue to the next entry.
fn preflight_vendor(
    root: &std::path::Path,
    entry: &SchemaEntry,
    first_warning: &mut Option<String>,
) -> Option<PreflightCheck> {
    // `mode() == Vendor` guarantees `schema_file` is `Some`.
    let rel = entry.schema_file.as_deref()?;
    let path = root.join(rel);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Some(PreflightCheck::Blocker(format!(
                "{}: cannot read schema_file `{}` ({e})",
                entry_label(&entry.name),
                path.display()
            )));
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Some(PreflightCheck::Blocker(format!(
                "{}: schema_file `{}` is not valid JSON ({e})",
                entry_label(&entry.name),
                path.display()
            )));
        }
    };

    let id = json.get("$id").and_then(serde_json::Value::as_str);
    if let Err(e) = manifest::check_id(id) {
        return Some(PreflightCheck::Blocker(format!(
            "{}: {e}",
            entry_label(&entry.name)
        )));
    }

    if let Some(schema_url) = json.get("$schema").and_then(serde_json::Value::as_str)
        && manifest::classify_dialect(schema_url) == manifest::Dialect::Unknown
    {
        first_warning.get_or_insert(format!(
            "{}: unrecognized `$schema` dialect `{schema_url}` \
             (SchemaStore CI may reject it)",
            entry_label(&entry.name)
        ));
    }

    None
}

/// External-entry preflight: validate the `url` is a well-formed http(s) URL.
///
/// Reachability is deliberately NOT probed: anodizer may be releasing the very
/// site that will host the schema, so the URL is legitimately not live until
/// after this release completes. An unreachable (or here, merely malformed)
/// external URL is therefore at most a warning.
fn preflight_external(entry: &SchemaEntry, first_warning: &mut Option<String>) {
    // `mode() == External` guarantees `url` is `Some`.
    let Some(url) = entry.url.as_deref() else {
        return;
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        first_warning.get_or_insert(format!(
            "{}: url `{url}` is not a well-formed http(s) URL",
            entry_label(&entry.name)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{SchemaEntry, SchemastoreConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    #[test]
    fn preflight_passes_with_no_schemas() {
        let ctx = TestContextBuilder::new().build();
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_file_missing() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/does-not-exist.json".into()),
                description: Some("Anodizer config".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("does-not-exist.json"), "{msg}");
            }
            other => panic!("expected Blocker for missing vendor file, got {other:?}"),
        }
    }

    #[test]
    fn preflight_warns_on_malformed_external_url() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("ftp://example.com/a.json".into()),
                description: Some("Anodizer config".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("http(s)"), "{msg}");
            }
            other => panic!("expected Warning for malformed url, got {other:?}"),
        }
    }

    #[test]
    fn preflight_passes_for_well_formed_external_url() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                description: Some("Anodizer config".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_on_bad_description() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                description: Some("a schema for stuff".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
            }
            other => panic!("expected Blocker for bad description, got {other:?}"),
        }
    }

    #[test]
    fn preflight_blocks_on_bad_derived_description() {
        use anodizer_core::config::MetadataConfig;
        let mut ctx = TestContextBuilder::new().build();
        // No explicit `description:` — the derived project description (which
        // contains the banned word "schema") must be validated at preflight,
        // not slip through to fail mid-publish.
        ctx.config.metadata = Some(MetadataConfig {
            description: Some("a schema for configs".into()),
            ..Default::default()
        });
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("derived"), "{msg}");
            }
            other => panic!("expected Blocker for bad derived description, got {other:?}"),
        }
    }

    #[test]
    fn preflight_passes_for_clean_derived_description() {
        use anodizer_core::config::MetadataConfig;
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.metadata = Some(MetadataConfig {
            description: Some("Rust release-automation configuration".into()),
            ..Default::default()
        });
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_skips_disabled_entries() {
        use anodizer_core::config::StringOrBool;
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/does-not-exist.json".into()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }],
            ..Default::default()
        };
        // The would-be Blocker (missing vendor file) must be skipped.
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    /// Write `body` to `<root>/schemas/anodizer.schema.json` and return a
    /// context whose `project_root` is `root` with a single vendor entry
    /// pointing at it. Used by the vendor-on-disk preflight tests.
    fn vendor_ctx_with_schema(root: &std::path::Path, body: &str) -> Context {
        let schemas_dir = root.join("schemas");
        std::fs::create_dir_all(&schemas_dir).expect("mkdir schemas");
        std::fs::write(schemas_dir.join("anodizer.schema.json"), body).expect("write schema");
        let mut ctx = TestContextBuilder::new()
            .project_root(root.to_path_buf())
            .build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/anodizer.schema.json".into()),
                description: Some("Anodizer config".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        ctx
    }

    #[test]
    fn preflight_passes_for_valid_vendor_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://example.com/anodizer.schema.json",
  "type": "object"
}"#,
        );
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_missing_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object"
}"#,
        );
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("$id"), "{msg}");
            }
            other => panic!("expected Blocker for missing $id, got {other:?}"),
        }
    }

    #[test]
    fn preflight_warns_for_unknown_dialect_vendor_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "https://example.com/not-a-known-dialect#",
  "$id": "https://example.com/anodizer.schema.json",
  "type": "object"
}"#,
        );
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("dialect"), "{msg}");
            }
            other => panic!("expected Warning for unknown dialect, got {other:?}"),
        }
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_not_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(dir.path(), "this is not json {");
        match preflight_checks(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("not valid JSON"), "{msg}");
            }
            other => panic!("expected Blocker for non-JSON schema, got {other:?}"),
        }
    }

    /// Preflight probes the fork with the token the publish pushes with and
    /// `SchemaStore/schemastore` with the token the publish opens the pull
    /// request with, each chosen by the resolvers the publish calls, and a
    /// pull request token template that does not render is a blocker.
    #[test]
    fn preflight_probes_the_tokens_the_publish_sends() {
        use super::super::publish::TOKEN_ENV_VAR;
        use anodizer_core::config::{PullRequestConfig, RepositoryConfig};
        use anodizer_core::test_helpers::scripted_responder::{
            ScriptedRoute, spawn_scripted_responder,
        };

        let body = r#"{"permissions":{"push":true}}"#;
        let ok: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let routes = vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/me/schemastore",
                response: ok,
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/SchemaStore/schemastore",
                response: ok,
                times: None,
            },
        ];
        let (addr, log) = spawn_scripted_responder(routes);
        let repo = |pr_token: &str| RepositoryConfig {
            owner: Some("me".into()),
            name: Some("schemastore".into()),
            pull_request: Some(PullRequestConfig {
                token: Some(pr_token.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx_with = |repo: RepositoryConfig| {
            let mut ctx = TestContextBuilder::new()
                .sealed_env()
                .env("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
                .env(TOKEN_ENV_VAR, "push-tok")
                .env("GITHUB_TOKEN", "ladder-tok")
                .build();
            ctx.template_vars_mut().set_env("PR_TOKEN", "pr-tok");
            ctx.config.schemastore = SchemastoreConfig {
                repository: Some(repo),
                schemas: vec![SchemaEntry {
                    name: "Anodizer".into(),
                    file_match: vec![".anodizer.yaml".into()],
                    url: Some("https://example.com/a.json".into()),
                    description: Some("Anodizer config".into()),
                    ..Default::default()
                }],
                ..Default::default()
            };
            ctx
        };

        let ctx = ctx_with(repo("{{ .Env.PR_TOKEN }}"));
        let configured = ctx.config.schemastore.repository.as_ref();
        let push = crate::util::resolve_repo_token(&ctx, configured, Some(TOKEN_ENV_VAR))
            .expect("renders")
            .expect("a push token");
        let pr = crate::util::resolve_pr_token(&ctx, configured, Some(TOKEN_ENV_VAR))
            .expect("renders")
            .expect("a pull request token");
        assert_eq!((push.as_str(), pr.as_str()), ("push-tok", "pr-tok"));
        assert!(matches!(
            preflight_checks(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
        let auth = |path: &str| -> Vec<String> {
            log.lock()
                .unwrap()
                .iter()
                .filter(|r| r.method == "GET" && r.path == path)
                .filter_map(|r| r.header("Authorization").map(str::to_string))
                .collect()
        };
        assert_eq!(
            auth("/repos/me/schemastore"),
            vec![format!("Bearer {push}")]
        );
        assert_eq!(
            auth("/repos/SchemaStore/schemastore"),
            vec![format!("Bearer {pr}")]
        );

        let broken = ctx_with(repo("{{ Undefined }}"));
        match preflight_checks(&broken).expect("preflight ok") {
            PreflightCheck::Blocker(m) => {
                assert!(m.contains("pull_request.token does not render"), "{m}")
            }
            other => panic!("expected a blocker for an unrenderable token, got {other:?}"),
        }
    }
}
