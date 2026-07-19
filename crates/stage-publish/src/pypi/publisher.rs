//! `PypiPublisher` — Submitter-group `Publisher` impl that assembles native
//! binary wheels (plus an optional maturin sdist) and uploads them via the
//! legacy (twine-protocol) API.
//!
//! Classification:
//! * **Group**: Submitter — a PyPI filename is an immutable registry slot
//!   that can NEVER be re-uploaded (even after deletion), so pypi belongs
//!   with the other one-way doors (cargo, chocolatey, winget) whose landed
//!   publish burns the version. This is what arms the rollback guard: a
//!   landed pypi upload counts toward `irreversibly_published`, refusing a
//!   same-version re-cut that would silently `skip_existing` the stale
//!   wheels. (It is NOT Manager: Manager is server-side-deletable
//!   package-manager state — homebrew/scoop/nix — which pypi is not.)
//! * **Required default**: `true` — a failed PyPI publish is load-bearing
//!   for users who install via `pip install`; the operator should know the
//!   release is half-shipped.
//! * **Rollback scope**: none. A published filename can NEVER be
//!   re-uploaded, even after deletion — PyPI uploads are a one-way door
//!   (like cargo and npm). Rollback is warn-only.
//!
//! Evidence: one [`PypiFileSnapshot`] per file offered to the index —
//! uploaded files and `skip_existing` idempotent skips both appear (the
//! skip flagged), so the run report shows exactly what is live.

use std::time::Duration;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::PypiConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::pep::{normalize_project_name, semver_to_pep440, validate_project_name};
use super::upload::{DEFAULT_REPOSITORY, FileType, UploadOutcome, upload_file};
use super::wheel::{WheelSpec, build_wheel, inspect_binary, platform_tag};

#[cfg(test)]
pub(crate) use super::preflight::body_lists_version;
pub use super::preflight::pypi_version_live;
pub(crate) use super::preflight::{
    platform_tag_collision_check, simple_index_lists_version, version_probe,
};

simple_publisher!(
    PypiPublisher,
    "pypi",
    anodizer_core::PublisherGroup::Submitter,
    true,
    None,
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields have no
/// slot to land in.
pub(crate) type PypiFileSnapshot = anodizer_core::publish_evidence::PypiFileSnapshot;

/// Env var fallback ladder for the upload token: `PYPI_TOKEN`, then
/// maturin's own `MATURIN_PYPI_TOKEN` so a project migrating from
/// `maturin publish` keeps its existing secret name.
pub(crate) const TOKEN_ENV_VARS: [&str; 2] = ["PYPI_TOKEN", "MATURIN_PYPI_TOKEN"];

/// Resolve the upload token: `cfg.token` (templated) wins; otherwise the
/// first non-empty env var from [`TOKEN_ENV_VARS`]. Empty string when all
/// are unset — the caller surfaces a clear "missing token" error.
pub(crate) fn resolve_token(ctx: &Context, cfg: &PypiConfig) -> Result<String> {
    crate::publisher_helpers::resolve_token_with_ladder(
        ctx,
        cfg.token.as_deref(),
        "pypi: render token template",
        &TOKEN_ENV_VARS,
    )
}

/// Resolve the credential sent as the `__token__` Basic-auth password, per the
/// entry's [`PypiAuthMode`]: an explicit/env token, or a freshly-minted
/// Trusted-Publishing token from the ambient GitHub Actions OIDC identity.
pub(crate) fn resolve_upload_credential(
    ctx: &Context,
    cfg: &PypiConfig,
    repository: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    label: &str,
    log: &StageLogger,
) -> Result<String> {
    use anodizer_core::config::PypiAuthMode;
    match cfg.auth {
        PypiAuthMode::Token => {
            let token = resolve_token(ctx, cfg)?;
            if token.is_empty() {
                bail!(
                    "pypi: auth=token requires an API token to upload to {} (entry '{}'). \
                     Set $PYPI_TOKEN (or $MATURIN_PYPI_TOKEN) or `pypis[].token`.",
                    repository,
                    label
                );
            }
            Ok(token)
        }
        // Strict Trusted Publishing: never fall back to a token, so a
        // misconfigured publisher fails the release loudly. The token field is
        // documented "Unused when auth: oidc", so it is never resolved here — a
        // malformed inline `token:` template must not abort an OIDC-only run.
        PypiAuthMode::Oidc => {
            super::oidc::mint_trusted_publishing_token(ctx, repository, policy, log)
        }
        // A token when present, else Trusted Publishing when an OIDC context is
        // available, else a hard error naming both paths. A `token:` template
        // that fails to render must NOT abort the run when OIDC is available:
        // auto's contract is "use whatever credential the environment offers",
        // so a token-render error is routed around to Trusted Publishing and
        // surfaced only when there is no OIDC fallback to take.
        PypiAuthMode::Auto => match resolve_token(ctx, cfg) {
            Ok(token) if !token.is_empty() => Ok(token),
            token_result => {
                if super::oidc::oidc_context_available(ctx) {
                    super::oidc::mint_trusted_publishing_token(ctx, repository, policy, log)
                } else {
                    // No OIDC fallback: surface a token-render error if there
                    // was one, else the no-credential guidance.
                    token_result?;
                    bail!(
                        "pypi: no credential available to upload to {} (entry '{}'). \
                         Set $PYPI_TOKEN (or $MATURIN_PYPI_TOKEN) or `pypis[].token`, or run \
                         under GitHub Actions with id-token: write for Trusted Publishing.",
                        repository,
                        label
                    );
                }
            }
        },
    }
}

/// The crate this entry is scoped to: the first `ids:` entry (the entry's
/// selected crate) when set, else the primary crate, else the project name.
///
/// Every per-entry identity — the PyPI project name fallback and the
/// summary/license/homepage METADATA fallbacks — resolves through THIS crate,
/// so an entry with `ids: ["other-crate"]` publishes other-crate's binary
/// under other-crate's metadata instead of the primary crate's (the npm
/// optional-deps publisher scopes the same way).
pub(crate) fn entry_crate_name(ctx: &Context, cfg: &PypiConfig) -> String {
    static_entry_crate_name(&ctx.config, cfg)
}

/// Context-free form of [`entry_crate_name`] for failure-recovery tooling
/// (`tag rollback`'s burn probe): resolve the crate a `pypis:` entry versions
/// from the config alone — its first non-empty `ids` entry, else the primary
/// crate name, else the project name. No render context is consulted, so it
/// works outside a release run where the guard maps a tag's version onto the
/// pypi project.
pub fn static_entry_crate_name(config: &anodizer_core::config::Config, cfg: &PypiConfig) -> String {
    cfg.ids
        .as_ref()
        .and_then(|ids| ids.iter().find(|id| !id.is_empty()))
        .cloned()
        .or_else(|| config.primary_crate_name().map(str::to_string))
        .unwrap_or_else(|| config.project_name.clone())
}

/// Static (context-free) PyPI project name for the rollback burn probe:
/// `cfg.name` when it is not a template expression, else `crate_name`.
/// Returns `None` when `cfg.name` is templated — outside a release run there
/// is nothing to render it with, and a destructive rollback that cannot name
/// the immutable project it would orphan must fail closed rather than probe a
/// guessed name. Mirrors [`resolve_name`] without a render context, the same
/// way winget's `static_package_identifier` mirrors its publisher's id
/// resolution.
///
/// Public for the same reason as [`crate::cargo::published_on_crates_io`]:
/// `tag rollback`'s published-state guard must name the same project the
/// publisher would upload.
pub fn static_project_name(crate_name: &str, cfg: &PypiConfig) -> Option<String> {
    match cfg.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) if n.contains("{{") => None,
        Some(n) => Some(n.to_string()),
        None => Some(crate_name.to_string()),
    }
}

/// Static (context-free) upload repository for the rollback burn probe:
/// `cfg.index_url` when set and not templated, else the production PyPI
/// default. Returns `None` when `cfg.index_url` is a template expression —
/// its host cannot be known outside a release run, so the guard cannot map it
/// to an index endpoint and must fail closed.
pub fn static_repository(cfg: &PypiConfig) -> Option<String> {
    match cfg
        .index_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(r) if r.contains("{{") => None,
        Some(r) => Some(r.to_string()),
        None => Some(DEFAULT_REPOSITORY.to_string()),
    }
}

/// Resolve the display-form project name: `cfg.name` else the entry's scoped
/// crate name.
pub(crate) fn resolve_name(ctx: &Context, cfg: &PypiConfig) -> String {
    cfg.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| entry_crate_name(ctx, cfg))
}

/// Resolve the (templated) repository URL, defaulting to production PyPI.
pub(crate) fn resolve_repository(ctx: &Context, cfg: &PypiConfig) -> Result<String> {
    match cfg.index_url.as_deref().filter(|r| !r.is_empty()) {
        Some(raw) => ctx
            .render_template(raw)
            .context("pypi: render index_url template"),
        None => Ok(DEFAULT_REPOSITORY.to_string()),
    }
}

/// Wheel mtime seed — the shared reproducible-mtime ladder
/// ([`Context::resolve_reproducible_mtime`]) that the archive stage also
/// uses, so a wheel and an archive built in the same run pin to the SAME
/// timestamp (a reproducible build prefers the commit timestamp; otherwise
/// `SOURCE_DATE_EPOCH` then the commit timestamp).
fn wheel_mtime(ctx: &Context) -> Option<u64> {
    ctx.resolve_reproducible_mtime()
}

/// Build the [`WheelSpec`] metadata shared by every file of one entry,
/// honouring the project-metadata fallbacks (summary ← `metadata.description`,
/// homepage ← `metadata.homepage`, license ← `metadata.license`).
pub(crate) fn build_spec_base(
    ctx: &Context,
    cfg: &PypiConfig,
    name: &str,
    version: &str,
    crate_name: &str,
) -> Result<WheelSpec> {
    // Template errors PROPAGATE: these fields land in immutable published
    // METADATA, so a broken template must abort the release rather than ship
    // its raw source (every other render in this file propagates the same
    // way).
    let render = |field: &str, s: &str| -> Result<String> {
        ctx.render_template(s)
            .with_context(|| format!("pypi: render {field} template"))
    };
    let render_opt = |field: &'static str, v: Option<&str>| -> Result<Option<String>> {
        v.map(|s| render(field, s)).transpose()
    };
    let summary = render_opt("summary", cfg.summary.as_deref())?.or_else(|| {
        ctx.config
            .meta_description_for(crate_name)
            .map(str::to_string)
    });
    let explicit_description = render_opt("description", cfg.description.as_deref())?;
    let description = explicit_description.clone().or_else(|| summary.clone());
    // Default the content-type to Markdown only for an explicitly-configured
    // `description` body — without a `Description-Content-Type` header PyPI
    // renders the body as raw plaintext. A `summary` fallback is a plain
    // one-liner and must NOT be tagged Markdown (its underscores/asterisks are
    // literal). An explicit config value always wins.
    let description_content_type = match cfg.description_content_type.as_deref() {
        Some(ct) => Some(render("description_content_type", ct)?),
        None if explicit_description.is_some() => Some("text/markdown".to_string()),
        None => None,
    };
    // `Project-URL` links, rendered (URLs may be templated) and kept in the
    // BTreeMap's sorted-by-label order for a byte-stable wheel.
    let project_urls = cfg
        .project_urls
        .iter()
        .flatten()
        .map(|(label, url)| render("project_urls", url).map(|u| (label.clone(), u)))
        .collect::<Result<Vec<_>>>()?;
    let homepage = render_opt("homepage", cfg.homepage.as_deref())?
        .or_else(|| ctx.config.meta_homepage_for(crate_name).map(str::to_string));
    // A `Homepage` label in project_urls is canonical; drop the separate
    // homepage line so PyPI never receives two `Project-URL: Homepage` entries
    // (Warehouse rejects duplicate Project-URL labels with HTTP 400).
    let homepage = homepage.filter(|_| {
        !project_urls
            .iter()
            .any(|(label, _)| label.eq_ignore_ascii_case("Homepage"))
    });
    Ok(WheelSpec {
        name: name.to_string(),
        version: version.to_string(),
        platform_tag: String::new(),
        metadata_version: "2.1".to_string(),
        bin_name: String::new(),
        description,
        description_content_type,
        author: render_opt("author", cfg.author.as_deref())?,
        author_email: render_opt("author_email", cfg.author_email.as_deref())?,
        project_urls,
        summary,
        license: render_opt("license", cfg.license.as_deref())?
            .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string)),
        homepage,
        requires_python: cfg.requires_python.clone(),
        keywords: cfg.keywords.clone().unwrap_or_default(),
        classifiers: cfg.classifiers.clone().unwrap_or_default(),
    })
}

/// One assembled distribution file awaiting upload.
struct DistFile {
    path: std::path::PathBuf,
    spec: WheelSpec,
    file_type: FileType,
}

/// Select this entry's binary artifacts: `UploadableBinary` (checksummed /
/// signed build outputs) falling back to raw `Binary`, plus any
/// `UniversalBinary` (which builds a `universal2` wheel), filtered by
/// `cfg.ids`.
fn select_binaries<'a>(
    ctx: &'a Context,
    cfg: &PypiConfig,
) -> Vec<(&'a anodizer_core::artifact::Artifact, bool)> {
    let mut binaries = ctx.artifacts.by_kind(ArtifactKind::UploadableBinary);
    if binaries.is_empty() {
        binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
    }
    let mut out: Vec<(&anodizer_core::artifact::Artifact, bool)> =
        binaries.into_iter().map(|a| (a, false)).collect();
    out.extend(
        ctx.artifacts
            .by_kind(ArtifactKind::UniversalBinary)
            .into_iter()
            .map(|a| (a, true)),
    );
    // Same semantics as npm's optional-deps `ids:` filter — binaries are
    // keyed by their owning crate (build outputs carry no archive-id
    // metadata to filter on).
    if let Some(ids) = cfg.ids.as_ref() {
        out.retain(|(a, _)| ids.iter().any(|id| id == &a.crate_name));
    }
    // `targets:` allowlist (orthogonal to `ids:`): keep only binaries whose
    // target triple is listed. A target left out of scope is silently dropped.
    if cfg.targets.as_ref().is_some_and(|t| !t.is_empty()) {
        out.retain(|(a, _)| {
            crate::publisher_helpers::target_in_allowlist(
                cfg.targets.as_ref(),
                a.target.as_deref().unwrap_or_default(),
            )
        });
    }
    // Microarch variant selection (same shape as homebrew/krew): an amd64
    // binary tagged with `amd64_variant` metadata is kept only when its
    // variant matches the selector (default `v1`); a 32-bit ARM binary tagged
    // with `arm_variant` is kept only when it matches `arm_variant` (when
    // set). A binary carrying no variant metadata always passes — it is the
    // baseline build.
    let amd64_variant = cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = cfg.arm_variant.as_deref();
    out.retain(|(a, _)| {
        let target = a.target.as_deref().unwrap_or_default();
        let (_, arch) = anodizer_core::target::map_target(target);
        if arch == "amd64" {
            return a
                .metadata
                .get("amd64_variant")
                .is_none_or(|v| v == amd64_variant);
        }
        if arch.starts_with("arm")
            && arch != "arm64"
            && let Some(want) = arm_variant
        {
            return a.metadata.get("arm_variant").is_none_or(|v| v == want);
        }
        true
    });
    out
}

/// Top-level publish entrypoint. Iterates each `pypis[]` entry, assembles
/// its wheels (+ optional sdist) into `<dist>/pypi/<entry>/`, and uploads
/// each file. `files` is an out-param so a mid-loop error still yields
/// evidence for what already landed.
pub(crate) fn publish_to_pypi(
    ctx: &Context,
    log: &StageLogger,
    files: &mut Vec<PypiFileSnapshot>,
) -> Result<()> {
    let entries = match ctx.config.pypis {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };
    let policy = ctx.retry_policy();
    let deadline = ctx.retry_deadline();

    for (idx, cfg) in entries.iter().enumerate() {
        let label = cfg.id.clone().unwrap_or_else(|| format!("pypis[{}]", idx));
        log.status(&format!("processing pypi project '{}'", label));

        // ---- Skip gates ----
        if let Some(skip) = cfg.skip.as_ref() {
            let off = skip
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("pypi: render skip template")?;
            if off {
                log.status("skipped pypi entry — skip evaluates true");
                continue;
            }
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("pypi entry '{}'", label),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped pypi entry — `if` condition evaluated falsy");
            continue;
        }

        // ---- Name + version forms ----
        let crate_name = entry_crate_name(ctx, cfg);
        let name = resolve_name(ctx, cfg);
        validate_project_name(&name)?;
        let normalized = normalize_project_name(&name);
        let version = semver_to_pep440(&ctx.version())?;
        let repository = resolve_repository(ctx, cfg)?;
        if cfg.sdist && cfg.sdist_manifest.as_deref().is_none_or(str::is_empty) {
            bail!(
                "pypi: entry '{}' sets `sdist: true` but `sdist_manifest` is unset — \
                 point it at the directory containing your pyproject.toml",
                label
            );
        }

        // ---- Assemble the file set ----
        let binaries = select_binaries(ctx, cfg);
        if binaries.is_empty() && !cfg.sdist {
            log.status(&format!(
                "no matching binaries for pypi project '{}' — nothing to upload",
                name
            ));
            continue;
        }
        // A `platform_tag_overrides` key that no built target matches is a typo
        // or a stale entry: without this guard the override silently falls
        // through to binary-inspection auto-detection, shipping a wheel tag the
        // operator thought they had pinned. Fail loudly instead.
        if let Some(overrides) = cfg.platform_tag_overrides.as_ref() {
            let built: std::collections::BTreeSet<&str> = binaries
                .iter()
                .filter_map(|(art, _)| art.target.as_deref())
                .filter(|t| !t.is_empty())
                .collect();
            let unknown: Vec<&str> = overrides
                .keys()
                .map(String::as_str)
                .filter(|k| !built.contains(k))
                .collect();
            if !unknown.is_empty() {
                bail!(
                    "pypi: entry '{}' platform_tag_overrides names target(s) it never \
                     builds: {} — remove the stale key or add a build for it (this \
                     entry builds: {})",
                    name,
                    unknown.join(", "),
                    built.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
        }
        let spec_base = build_spec_base(ctx, cfg, &name, &version, &crate_name)?;
        let staging = ctx.config.dist.join("pypi").join(&label);
        let mtime = wheel_mtime(ctx);

        let mut dist_files: Vec<DistFile> = Vec::new();
        let mut seen_tags: Vec<String> = Vec::new();
        for (art, universal) in &binaries {
            let target = art.target.as_deref().unwrap_or_default();
            if target.is_empty() {
                bail!(
                    "pypi: binary artifact '{}' has no target triple — cannot derive \
                     a wheel platform tag",
                    art.name
                );
            }
            let bytes = std::fs::read(&art.path)
                .with_context(|| format!("pypi: read binary '{}'", art.path.display()))?;
            let traits = inspect_binary(&bytes, *universal)?;
            // A per-target override wins verbatim over binary inspection: the
            // configured tag is used exactly as written, skipping glibc-floor
            // and Mach-O deployment-target detection for this target.
            let tag = match cfg
                .platform_tag_overrides
                .as_ref()
                .and_then(|m| m.get(target))
            {
                Some(over) => over.clone(),
                None => platform_tag(target, &traits)?,
            };
            if seen_tags.contains(&tag) {
                bail!(
                    "pypi: two binaries derive the same wheel platform tag '{}' — \
                     narrow `ids:` so each entry publishes one binary per platform",
                    tag
                );
            }
            seen_tags.push(tag.clone());
            let mut spec = spec_base.clone();
            spec.platform_tag = tag;
            spec.bin_name = art.name.clone();
            if ctx.is_dry_run() {
                log.status(&format!(
                    "(dry-run) would build + upload {}",
                    spec.filename()
                ));
                continue;
            }
            let path = build_wheel(&spec, &bytes, &staging, mtime, env!("CARGO_PKG_VERSION"))?;
            log.status(&format!(
                "built wheel {} ({})",
                spec.filename(),
                spec.platform_tag
            ));
            dist_files.push(DistFile {
                path,
                spec,
                file_type: FileType::Wheel,
            });
        }

        if cfg.sdist {
            let manifest_dir = ctx
                .render_template(cfg.sdist_manifest.as_deref().unwrap_or_default())
                .context("pypi: render sdist_manifest template")?;
            let sdist_out = staging.join("sdist");
            if ctx.is_dry_run() {
                log.status(&format!(
                    "(dry-run) would run: maturin sdist --manifest-path {}/pyproject.toml --out {}",
                    manifest_dir.trim_end_matches('/'),
                    sdist_out.display()
                ));
            } else {
                let path = super::sdist::build_sdist(ctx, &manifest_dir, &sdist_out, log)?;
                let sdist_name = path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                log.status(&format!("built sdist {}", sdist_name));
                // The upload form must echo the sdist's OWN PKG-INFO
                // metadata_version + version (maturin emits its own
                // Metadata-Version and the pyproject version), or Warehouse
                // 400s the form/PKG-INFO mismatch mid-release.
                let pkg_info = super::sdist::parse_pkg_info(&path)?;
                let mut spec = spec_base.clone();
                spec.platform_tag = "source".to_string();
                spec.version = pkg_info.version;
                spec.metadata_version = pkg_info.metadata_version;
                dist_files.push(DistFile {
                    path,
                    spec,
                    file_type: FileType::Sdist,
                });
            }
        }

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would upload {} file(s) to {}",
                binaries.len() + usize::from(cfg.sdist),
                repository
            ));
            continue;
        }

        // ---- Credential + upload ----
        let token = resolve_upload_credential(ctx, cfg, &repository, &policy, &label, log)?;
        let client = anodizer_core::http::blocking_client(Duration::from_secs(60))
            .context("pypi: build HTTP client")?;
        for f in &dist_files {
            let filename = f
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let outcome = upload_file(
                &client,
                &repository,
                &token,
                &normalized,
                &f.spec,
                f.file_type,
                &f.path,
                cfg.skip_existing,
                &policy,
                deadline,
                log,
            )?;
            let (sha256, skipped) = match outcome {
                UploadOutcome::Uploaded { sha256 } => {
                    log.status(&format!("uploaded {} → {}", filename, repository));
                    (sha256, false)
                }
                UploadOutcome::SkippedExisting { sha256 } => (sha256, true),
            };
            files.push(PypiFileSnapshot {
                filename,
                platform_tag: f.spec.platform_tag.clone(),
                sha256,
                repository: repository.clone(),
                skipped_existing: skipped,
            });
        }
        log.status(&format!(
            "pypi publish complete for '{}' ({} file(s))",
            name,
            dist_files.len()
        ));
    }
    Ok(())
}

/// Top-level `pypis:` entries whose `skip:`/`if:` evaluates active right
/// now. Shared by [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `preflight` keeps its own loop (it needs per-entry index-URL
/// resolution alongside the filter, not just a boolean).
fn active_pypi_configs(ctx: &Context) -> Vec<&anodizer_core::config::PypiConfig> {
    ctx.config
        .pypis
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

impl anodizer_core::Publisher for PypiPublisher {
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

    /// `true` — PyPI is a long-lived public registry where nightly version
    /// churn is unwanted (the same reasoning as npm: every nightly would
    /// permanently consume a version/filename on an index users resolve
    /// against).
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_pypi_configs(ctx).is_empty()
    }

    /// Per active entry: the upload token (a templated `cfg.token`'s env
    /// refs, else the `PYPI_TOKEN` / `MATURIN_PYPI_TOKEN` any-of fallback
    /// ladder), plus the `maturin` CLI when `sdist: true`.
    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let active = active_pypi_configs(ctx);
        let mut out = Vec::new();
        if active.iter().any(|e| e.sdist) {
            out.push(anodizer_core::EnvRequirement::Tool {
                name: "maturin".to_string(),
            });
        }
        for entry in &active {
            use anodizer_core::config::PypiAuthMode;
            // The token requirement in isolation: a templated token's env refs,
            // else the PYPI_TOKEN/MATURIN_PYPI_TOKEN any-of fallback ladder. A
            // literal inline token declares nothing (`None`).
            let token_req = match entry.token.as_deref().filter(|t| !t.is_empty()) {
                Some(_) => crate::publisher_helpers::secret_requirement(
                    entry.token.as_deref(),
                    TOKEN_ENV_VARS[0],
                ),
                None => Some(anodizer_core::EnvRequirement::EnvAnyOf {
                    vars: TOKEN_ENV_VARS.iter().map(|s| s.to_string()).collect(),
                }),
            };
            let oidc_vars = || -> Vec<String> {
                super::oidc::OIDC_ENV_VARS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            };
            match entry.auth {
                // Token-only: the token is mandatory.
                PypiAuthMode::Token => out.extend(token_req),
                // Strict OIDC: require the GitHub Actions request pair; never a
                // token (the run path refuses to fall back to one).
                PypiAuthMode::Oidc => {
                    out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: oidc_vars() })
                }
                // Auto resolves at publish time (token if present, else OIDC).
                // Preflight applies only a COARSE token-OR-OIDC gate so it
                // catches the zero-credential case without false-failing a valid
                // OIDC-only run.
                PypiAuthMode::Auto => match token_req {
                    // Literal inline token → the credential is always present.
                    None => {}
                    Some(anodizer_core::EnvRequirement::EnvAnyOf { vars })
                    | Some(anodizer_core::EnvRequirement::EnvAllOf { vars }) => {
                        let mut any = vars;
                        any.extend(oidc_vars());
                        out.push(anodizer_core::EnvRequirement::EnvAnyOf { vars: any });
                    }
                    // secret_requirement only yields EnvAllOf/None today; forward
                    // any other shape verbatim rather than dropping the gate.
                    Some(other) => out.push(other),
                },
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // Accumulate every file that lands BEFORE a mid-loop failure so the
        // evidence still names the already-live (one-way) uploads. On Err the
        // evidence is built from the partial set, the Failed outcome is
        // recorded, and Ok(evidence) is returned — bubbling Err would make
        // dispatch drop the evidence and orphan the landed files from the
        // run report.
        let mut files: Vec<PypiFileSnapshot> = Vec::new();
        let publish_err = publish_to_pypi(ctx, &log, &mut files).err();

        let mut evidence = anodizer_core::PublishEvidence::new("pypi");
        if let Some(first) = files.first() {
            evidence.primary_ref = Some(format!(
                "{}#{}",
                first.repository.trim_end_matches('/'),
                first.filename
            ));
        }
        if !files.is_empty() {
            evidence.extra = anodizer_core::PublishEvidenceExtra::Pypi(
                anodizer_core::publish_evidence::PypiExtra { pypi_files: files },
            );
        }
        if let Some(e) = publish_err {
            log.error(&format!("pypi: publish failed: {e:#}"));
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Failed(format!("{e:#}")));
        }
        Ok(evidence)
    }

    /// Warn-only: a published filename can never be re-uploaded (deleting a
    /// file does not free its name), so there is nothing programmatic to
    /// undo — the operator must fix forward to the next version.
    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let files = match &evidence.extra {
            anodizer_core::PublishEvidenceExtra::Pypi(p) => p.pypi_files.clone(),
            _ => Vec::new(),
        };
        if files.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "pypi",
                "uploaded files",
            ));
            return Ok(());
        }
        for f in files.iter().filter(|f| !f.skipped_existing) {
            log.warn(&format!(
                "pypi rollback cannot undo '{}' on {} — PyPI uploads are one-way \
                 (a filename can never be re-uploaded, even after deletion); \
                 fix forward to the next version",
                f.filename, f.repository
            ));
        }
        Ok(())
    }

    /// Live pre-publish gate. Per active entry:
    ///
    /// * project name illegal / version unmappable to PEP 440 / `sdist: true`
    ///   without `sdist_manifest` ⇒ Blocker (the publish cannot proceed);
    /// * `<name>==<version>` already on the index ⇒ Warning — the run path's
    ///   `skip_existing` handles the re-run case, so this mirrors the other
    ///   Manager publishers' duplicate-version warn rather than blocking.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{merge, probe_version_published};
        use anodizer_core::PreflightCheck;

        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let mut acc = PreflightCheck::Pass;
        for cfg in ctx.config.pypis.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            ) {
                continue;
            }
            let name = resolve_name(ctx, cfg);
            if let Err(e) = validate_project_name(&name) {
                acc = merge(acc, PreflightCheck::Blocker(format!("{e:#}")));
                continue;
            }
            let version = match semver_to_pep440(&ctx.version()) {
                Ok(v) => v,
                Err(e) => {
                    acc = merge(acc, PreflightCheck::Blocker(format!("{e:#}")));
                    continue;
                }
            };
            if cfg.sdist && cfg.sdist_manifest.as_deref().is_none_or(str::is_empty) {
                acc = merge(
                    acc,
                    PreflightCheck::Blocker(
                        "pypi: `sdist: true` requires `sdist_manifest` to point at the \
                         directory containing pyproject.toml"
                            .to_string(),
                    ),
                );
            }
            let repository = match resolve_repository(ctx, cfg) {
                Ok(r) => r,
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Blocker(format!(
                            "pypi: repository template could not be rendered: {e:#}"
                        )),
                    );
                    continue;
                }
            };
            acc = merge(
                acc,
                crate::publisher_helpers::targets_allowlist_check(
                    ctx,
                    cfg.targets.as_ref(),
                    cfg.ids.as_ref(),
                    "pypi",
                ),
            );
            acc = merge(acc, platform_tag_collision_check(ctx, cfg));
            let normalized = normalize_project_name(&name);
            let already_published = match version_probe(&repository, &normalized, &version) {
                Some((url, false)) => probe_version_published(
                    &url,
                    "preflight: pypi version probe",
                    &policy,
                    &ctx.logger("preflight"),
                )
                .then_some(url),
                Some((url, true)) => {
                    simple_index_lists_version(&url, &normalized, &version).then_some(url)
                }
                None => None,
            };
            if let Some(url) = already_published {
                acc = merge(
                    acc,
                    PreflightCheck::Warning(format!(
                        "pypi: {}=={} already appears on the index ({}); existing files \
                         will be skipped (`skip_existing`), and a changed file with the \
                         same name can never replace the published one",
                        normalized, version, url
                    )),
                );
            }
        }
        Ok(acc)
    }
}
