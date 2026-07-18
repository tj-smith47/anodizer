use super::*;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, GitHubConfig};
use anodizer_core::context::Context;
use anodizer_core::git;
use anodizer_core::log::StageLogger;
use anodizer_core::scm::{self, ScmTokenType};
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Auto-infer `project_name` from Cargo.toml when not set in config.
///
/// The project name is inferred from Cargo.toml,
/// go.mod, or the git remote. We mirror the Cargo.toml branch here so
/// every pipeline command (release, build, check, continue) resolves the
/// project name consistently.
pub fn infer_project_name(config: &mut Config, log: &StageLogger) {
    if !config.project_name.is_empty() {
        return;
    }
    if let Ok(cargo_toml) = std::fs::read_to_string("Cargo.toml")
        && let Ok(doc) = cargo_toml.parse::<toml_edit::DocumentMut>()
        && let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    {
        config.project_name = name.to_string();
        log.verbose(&format!("inferred project_name '{}' from Cargo.toml", name));
    }
}

/// Auto-detect the GitHub owner/name from the git remote and fill in any crate
/// release configs that are missing the `github` section.
pub fn auto_detect_github(config: &mut Config, log: &StageLogger) {
    // Resolve the slug once from the origin remote (no config override here —
    // this fills the per-crate override precisely when it is absent).
    let detected_github = git::resolve_github_slug(None, None).ok();
    // Raw chained walk (not `crate_universe()`): this is a mutation pass and
    // the universe walker only hands out shared borrows. Filling every entry
    // as written (shadowed ones included) is also correct here since dedup
    // happens at read time.
    let crates_iter = config.crates.iter_mut().chain(
        config
            .workspaces
            .iter_mut()
            .flatten()
            .flat_map(|w| w.crates.iter_mut()),
    );
    for crate_cfg in crates_iter {
        if let Some(ref mut release) = crate_cfg.release
            && release.github.is_none()
        {
            if let Some(slug) = &detected_github {
                release.github = Some(GitHubConfig {
                    owner: slug.owner().to_string(),
                    name: slug.name().to_string(),
                    token: None,
                });
            } else {
                log.warn("could not auto-detect GitHub repo from git remote");
            }
        }
    }
}

/// Perform the standard context setup sequence shared by all pipeline commands.
///
/// This encapsulates the boilerplate that every pipeline entry point
/// (release, publish, announce, continue) must run after constructing a
/// `Context`:
///   1. Resolve SCM token type from config/environment
///   2. Populate time template variables
///   3. Populate runtime template variables (host OS/arch + rustc version)
///   4. Load environment variables and `.env` files
///   5. Resolve git context (tag discovery, git info)
pub fn setup_context(ctx: &mut Context, config: &Config, log: &StageLogger) -> Result<()> {
    resolve_scm_token_type(ctx, config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    // Default the `IsPrepare` template var to `"false"` for every
    // command that flows through `setup_context`. The release command
    // overrides this when `--prepare` is passed (see
    // `commands/release/mod.rs`). Setting it unconditionally avoids a
    // "missing key" footgun in user templates that branch on
    // `{{ if IsPrepare }}`.
    ctx.template_vars_mut().set_bool("IsPrepare", false);
    setup_env(ctx, config, log)?;
    resolve_git_context(ctx, config, log)?;
    Ok(())
}

/// Resolve the SCM token type and token value from config and environment.
///
/// This sets `ctx.token_type` based on priority (highest first):
/// 1. `config.force_token` — explicit user config (`force_token: gitlab`)
/// 2. `ANODIZER_FORCE_TOKEN` env var — e.g. `github`, `gitlab`, `gitea`
/// 3. `GORELEASER_FORCE_TOKEN` env var — compat fallback
/// 4. Environment variable presence — `GITLAB_TOKEN` → GitLab, `GITEA_TOKEN` → Gitea
/// 5. Default — GitHub
///
/// It also resolves the token value into `ctx.options.token` (if not already
/// set by a CLI flag) from the appropriate environment variable:
/// - GitLab: `GITLAB_TOKEN`
/// - Gitea: `GITEA_TOKEN`
/// - GitHub: `ANODIZER_GITHUB_TOKEN` or `GITHUB_TOKEN`
pub fn resolve_scm_token_type(ctx: &mut Context, config: &Config) {
    // Detect which SCM backend to use from environment variables. The
    // EnvSource indirection lets tests build a `Context` via
    // `TestContextBuilder::env(...)` and drive every branch without
    // mutating process env.
    let env_hint = if ctx.env_var("GITLAB_TOKEN").is_some() {
        Some("gitlab")
    } else if ctx.env_var("GITEA_TOKEN").is_some() {
        Some("gitea")
    } else {
        None
    };

    // Resolution priority: explicit `release.provider:` (cross-platform
    // cross-platform publishing) > top-level `force_token:` > env-var
    // detection. `release.provider:` makes the cross-platform case
    // declarative: a project that lives on GitLab but publishes to
    // GitHub declares `provider: github` and the token detection
    // honours it without needing the user to clear `GITLAB_TOKEN`.
    let provider_force = config.release.as_ref().and_then(|r| r.provider.clone());
    let force_token =
        provider_force.or_else(|| resolve_force_token_with_env(config, ctx.env_source()));

    ctx.token_type = scm::resolve_token_type(force_token.as_ref(), env_hint);

    // Resolve the token value if not already provided via CLI flag.
    if ctx.options.token.is_none() {
        ctx.options.token = match ctx.token_type {
            ScmTokenType::GitLab => ctx.env_var("GITLAB_TOKEN"),
            ScmTokenType::Gitea => ctx.env_var("GITEA_TOKEN"),
            // Route through the canonical resolver so an empty
            // `GITHUB_TOKEN=""` (the shape GH Actions gives a missing secret)
            // falls through to the next source instead of being taken as a
            // real token.
            ScmTokenType::GitHub => {
                anodizer_core::git::resolve_github_token_with_env(None, &|key| ctx.env_var(key))
            }
        };
    }
}

/// Load config, auto-detect GitHub, build a `Context`, and rehydrate
/// artifacts from `dist/` — the shared prelude for the `publish`,
/// `announce`, and (no-`--merge` branch of) `continue` commands.
///
/// Returns `(config, ctx, dist)` so the caller can drive the publish /
/// announce pipeline. `ctx_opts` is assembled by the caller so each
/// command supplies its own `skip_stages` / `merge` / `token` overlay.
///
/// Side effect: emits a `log.status("loaded N artifact(s) from <dist>")`
/// line after rehydration so the operator sees the artifact count at
/// the same point in every "resume from dist" command.
pub fn init_publish_stage_ctx(
    config_override: Option<&Path>,
    ctx_opts: anodizer_core::context::ContextOptions,
    dist_override: Option<&Path>,
    infer_project: bool,
    log: &StageLogger,
) -> Result<(Config, Context, std::path::PathBuf)> {
    let config_path = crate::pipeline::find_config_with_logger(config_override, Some(log))?;
    // Bare load: the submitter advisories are emitted below once `ctx` carries
    // the `--skip` / `--publishers` selection surface, so a deselected
    // publisher's advisory is suppressed instead of printed as noise.
    let mut config = crate::pipeline::load_config(&config_path)?;
    if infer_project {
        infer_project_name(&mut config, log);
    }
    auto_detect_github(&mut config, log);

    let mut ctx = Context::new(config.clone(), ctx_opts);
    crate::pipeline::emit_config_advisories_filtered(&config, log, |name| {
        ctx.publisher_deselected(name)
    });
    setup_context(&mut ctx, &config, log)?;

    let dist = dist_override.unwrap_or(&config.dist).to_path_buf();
    load_artifacts_from_dist(&mut ctx, &dist)?;
    log.status(&format!(
        "loaded {} artifact(s) from {}",
        ctx.artifacts.all().len(),
        dist.display()
    ));

    Ok((config, ctx, dist))
}

/// Build the context for a `--merge`-mode command (`publish --merge`,
/// `announce --merge`, `continue --merge`).
///
/// Merge mode has no `dist/artifacts.json` yet — the per-shard loader
/// (`load_split_contexts_into` / `run_merge`) populates the artifact set
/// from `dist/<subdir>/context.json` files afterward. So the prelude can't
/// reuse [`init_publish_stage_ctx`] (which loads `dist/artifacts.json`);
/// it builds the context manually: find + load config, infer project name,
/// auto-detect GitHub, construct the context, resolve git, and populate the
/// metadata var. Returns `(config, ctx)` for the caller to drive into its
/// merge-specific loader.
pub fn init_merge_stage_ctx(
    config_override: Option<&Path>,
    ctx_opts: anodizer_core::context::ContextOptions,
    log: &StageLogger,
) -> Result<(Config, Context)> {
    let config_path = crate::pipeline::find_config_with_logger(config_override, Some(log))?;
    // Bare load: advisories emitted below once `ctx` carries the selection
    // surface (see `init_publish_stage_ctx`).
    let mut config = crate::pipeline::load_config(&config_path)?;
    infer_project_name(&mut config, log);
    auto_detect_github(&mut config, log);
    let mut ctx = Context::new(config.clone(), ctx_opts);
    crate::pipeline::emit_config_advisories_filtered(&config, log, |name| {
        ctx.publisher_deselected(name)
    });
    setup_context(&mut ctx, &config, log)?;
    ctx.populate_metadata_var()?;
    Ok((config, ctx))
}

/// Load artifacts from dist/artifacts.json into the context's artifact registry.
/// Used by `publish` and `announce` commands that run from a completed dist/.
pub fn load_artifacts_from_dist(ctx: &mut Context, dist: &Path) -> Result<()> {
    let artifacts_path = dist.join(anodizer_core::dist::ARTIFACTS_JSON);
    load_artifacts_from_manifest(ctx, dist, &artifacts_path)
}

/// Load artifacts from an explicitly-named manifest path under `dist/`.
/// Split from [`load_artifacts_from_dist`] so a sharded matrix can fold
/// in `artifacts-<shard>.json` files one at a time. `dist` is carried
/// only for the error message (caller-meaningful location).
pub fn load_artifacts_from_manifest(
    ctx: &mut Context,
    dist: &Path,
    manifest_path: &Path,
) -> Result<()> {
    if !manifest_path.exists() {
        anyhow::bail!(
            "no artifacts manifest found at {} (under {}). Run a full release or merge first.",
            manifest_path.display(),
            dist.display()
        );
    }

    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;

    #[derive(serde::Deserialize)]
    struct MetadataArtifact {
        kind: String,
        #[serde(default)]
        name: Option<String>,
        path: String,
        target: Option<String>,
        crate_name: String,
        #[serde(default)]
        metadata: HashMap<String, String>,
        #[serde(default)]
        size: Option<u64>,
    }

    let artifacts: Vec<MetadataArtifact> = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", manifest_path.display()))?;

    for a in artifacts {
        let kind = ArtifactKind::parse(&a.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", a.kind))?;
        // Re-anchor `./dist/<rel>` / `dist/<rel>` paths onto the caller-
        // supplied `dist` root. Stored paths reflect the harness worktree's
        // `dist/<file>` shape; per-crate publish-only consumes from
        // `./dist/<crate>/` and would otherwise hit the manifest path
        // verbatim (`./dist/<file>`) instead of `./dist/<crate>/<file>` and
        // trip `detect_missing_files`. Flat callers (`publish`, `announce`,
        // single-crate `publish-only`) pass `dist=./dist`, so the rewrite
        // is a no-op for them. Paths outside the dist root (raw
        // `.det-tmp/target/...` binaries surfaced as Binary artifacts) are
        // left alone.
        let path_str = a.path.as_str();
        let rewritten = if let Some(rel) = path_str
            .strip_prefix("./dist/")
            .or_else(|| path_str.strip_prefix("dist/"))
        {
            dist.join(rel)
        } else {
            std::path::PathBuf::from(path_str)
        };
        // Cross-shard cross-target artifacts (source archive, install.sh,
        // metadata.json — all `target: None`) appear in every shard's
        // manifest by design and are byte-deduped on disk by
        // `download-artifact merge-multiple`. Adding all N shard entries
        // here would emit N-1 "already registered" warnings per artifact
        // before `dedupe_targetless_duplicates` cleaned them up — noise
        // that doesn't reflect a real problem. Skip-add when the same
        // path is already in the registry and the entry is targetless.
        if a.target.is_none()
            && ctx
                .artifacts
                .all()
                .iter()
                .any(|existing| existing.target.is_none() && existing.path == rewritten)
        {
            continue;
        }
        ctx.artifacts.add(Artifact {
            kind,
            name: a.name.unwrap_or_default(),
            path: rewritten,
            target: a.target,
            crate_name: a.crate_name,
            metadata: a.metadata,
            size: a.size,
        });
    }

    Ok(())
}

/// Locate the Cargo workspace root for version-aware commands (`bump`, `tag`,
/// `changelog`).
///
/// When a `--config` override is supplied, walk UP from the config file's
/// directory to the first ancestor containing a `Cargo.toml`; this lets a
/// config living beside (or below) the manifest resolve the same root. With no
/// override, walk up from the current directory instead. Bails when no
/// `Cargo.toml` is found in either chain.
///
/// Unifying all three commands on this discovery means they behave identically
/// whether invoked from the workspace root or a subdirectory — the standalone
/// fallback of "cwd is the root" silently loaded the wrong manifests from a
/// subdir.
pub(crate) fn discover_workspace_root(config_override: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Always return an ABSOLUTE root: callers thread it into `git -C <root>`,
    // which fails on an empty/relative path. A config resolved cwd-relative
    // (e.g. the bare `.anodizer.yaml` auto-discovery returns) has an empty
    // parent whose ancestor walk yields `""`, so absolutize every candidate
    // against the cwd before returning.
    let absolutize = |p: &Path| -> PathBuf {
        if p.as_os_str().is_empty() {
            cwd.clone()
        } else if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    if let Some(p) = config_override {
        // Config override points at .anodizer.yaml; walk up until we find Cargo.toml.
        if let Some(dir) = p.parent() {
            for ancestor in dir.ancestors() {
                if absolutize(ancestor).join("Cargo.toml").is_file() {
                    return Ok(absolutize(ancestor));
                }
            }
        }
    }
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file() {
            return Ok(absolutize(ancestor));
        }
    }
    anyhow::bail!("no Cargo.toml found from {}", cwd.display());
}
