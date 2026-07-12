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
/// `cfg.repository` when set and not templated, else the production PyPI
/// default. Returns `None` when `cfg.repository` is a template expression —
/// its host cannot be known outside a release run, so the guard cannot map it
/// to an index endpoint and must fail closed.
pub fn static_repository(cfg: &PypiConfig) -> Option<String> {
    match cfg
        .repository
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
    match cfg.repository.as_deref().filter(|r| !r.is_empty()) {
        Some(raw) => ctx
            .render_template(raw)
            .context("pypi: render repository template"),
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
fn build_spec_base(
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
    Ok(WheelSpec {
        name: name.to_string(),
        version: version.to_string(),
        platform_tag: String::new(),
        metadata_version: "2.1".to_string(),
        bin_name: String::new(),
        description: render_opt("description", cfg.description.as_deref())?
            .or_else(|| summary.clone()),
        summary,
        license: render_opt("license", cfg.license.as_deref())?
            .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string)),
        homepage: render_opt("homepage", cfg.homepage.as_deref())?
            .or_else(|| ctx.config.meta_homepage_for(crate_name).map(str::to_string)),
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
            let tag = platform_tag(target, &traits)?;
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

        // ---- Token + upload ----
        let token = resolve_token(ctx, cfg)?;
        if token.is_empty() {
            bail!(
                "pypi: an API token is required to upload to {} (entry '{}'). \
                 Set $PYPI_TOKEN (or $MATURIN_PYPI_TOKEN) or `pypis[].token`.",
                repository,
                label
            );
        }
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

/// Derive the pre-publish duplicate-version probe URL for a repository.
///
/// The `*.pypi.org` upload hosts pair with a JSON API
/// (`https://pypi.org/pypi/<name>/<version>/json`, and the TestPyPI
/// equivalent); a custom index has no JSON API contract, so its PEP 503
/// `/simple/<name>/` page is probed instead (the simple index lists every
/// released filename). Returns `(url, expect_filename)` — when
/// `expect_filename` is `true`, a 200 only means "already published" if the
/// body names a file of this version (the JSON API is version-precise; a
/// simple-index page exists for ANY released version).
pub(crate) fn version_probe(
    repository: &str,
    normalized_name: &str,
    version: &str,
) -> Option<(String, bool)> {
    let url = reqwest::Url::parse(repository).ok()?;
    let host = url.host_str()?;
    if host == "upload.pypi.org" || host == "pypi.org" {
        return Some((
            format!("https://pypi.org/pypi/{normalized_name}/{version}/json"),
            false,
        ));
    }
    if host == "test.pypi.org" {
        return Some((
            format!("https://test.pypi.org/pypi/{normalized_name}/{version}/json"),
            false,
        ));
    }
    let origin = format!(
        "{}://{}{}",
        url.scheme(),
        host,
        match url.port() {
            Some(p) => format!(":{p}"),
            None => String::new(),
        }
    );
    Some((format!("{origin}/simple/{normalized_name}/"), true))
}

/// Best-effort probe of a PEP 503 simple-index page for a released file of
/// exactly `normalized_name` at `version`. Any failure (transport, non-200,
/// unreadable body) folds to `false` — the duplicate warning must never be
/// fabricated from a network blip.
fn simple_index_lists_version(url: &str, normalized_name: &str, version: &str) -> bool {
    let Ok(client) = anodizer_core::http::blocking_client(Duration::from_secs(10)) else {
        return false;
    };
    match client.get(url).send() {
        Ok(resp) if resp.status().is_success() => resp
            .text()
            .map(|body| body_lists_version(&body, normalized_name, version))
            .unwrap_or(false),
        _ => false,
    }
}

/// Live-index probe for `tag rollback`'s published-state guard: is
/// `<project>@<version>` already released on `repository`'s PyPI index?
/// `Ok(true)` = a released file exists (the version is BURNED — a PyPI
/// filename is a permanent index slot that can never be re-uploaded, even
/// after deletion), `Ok(false)` = positively absent, `Err` = the index could
/// not be consulted (a caller making a destructive rollback decision must FAIL
/// CLOSED on this, exactly like [`crate::cargo::published_on_crates_io`]).
///
/// Reuses [`version_probe`] to pick the version-precise JSON API for the
/// public PyPI hosts and the PEP 503 simple-index page for any other
/// PyPI-protocol repository, so the rollback guard and the publisher's own
/// duplicate-version detection can never disagree about what "already on the
/// index" means. HTTP stays in this crate; the CLI guard only wires the closure.
pub fn pypi_version_live(
    repository: &str,
    project_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let normalized = normalize_project_name(project_name);
    // The publisher uploads under the PEP 440 form (`semver_to_pep440`, mirrored
    // from the publish path); probe the SAME string so a pre-release or
    // build-metadata version (`v1.2.3-rc.1` → `1.2.3rc1`) is not read as
    // un-published and mistaken for a free slot. A version that cannot be
    // normalized fails closed (`Err`) — a destructive rollback must never
    // proceed on a version it cannot verify.
    let pep440 = semver_to_pep440(version)
        .with_context(|| format!("pypi: normalize version {version:?} for rollback burn probe"))?;
    let Some((url, expect_filename)) = version_probe(repository, &normalized, &pep440) else {
        bail!(
            "pypi: could not derive an index-probe URL for repository {repository:?} \
             (project '{normalized}' at {pep440})"
        );
    };
    if expect_filename {
        simple_index_lists_version_checked(&url, &normalized, &pep440, policy, log)
    } else {
        crate::publisher_preflight::probe_version_landing(
            &url,
            "rollback: pypi version probe",
            policy,
            log,
        )
    }
}

/// Fail-closed sibling of [`simple_index_lists_version`]: a definitive 404
/// (the project has no index page) folds to `Ok(false)`, a 200 parses the body
/// for exactly `normalized_name@version`, and any other outcome (transport
/// failure, 5xx) surfaces `Err` so the rollback guard never mistakes an
/// unreachable index for "not published". The best-effort variant is still
/// correct for the pre-publish duplicate *warning*, where an outage safely
/// folds to "no warning".
fn simple_index_lists_version_checked(
    url: &str,
    normalized_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    use anodizer_core::retry::{RetryLog, SuccessClass, http_status, retry_http_blocking};
    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("build HTTP client for pypi simple-index probe")?;
    match retry_http_blocking(
        RetryLog::new("rollback: pypi simple-index probe", log),
        policy,
        SuccessClass::Strict,
        |_| client.get(url).send(),
        |status, body| format!("{status}: {body}"),
    ) {
        Ok((_, body)) => Ok(body_lists_version(&body, normalized_name, version)),
        Err(err) if http_status(&err) == 404 => Ok(false),
        Err(err) => Err(err),
    }
}

/// True when a simple-index page body lists a distribution file whose parsed
/// name (PEP 503 normalized) and version EXACTLY equal the probe's.
///
/// The filenames are parsed and compared field-wise rather than substring-
/// matched: an unanchored `contains("foo-1.2.3")` false-positives `foo-1.2.30`
/// and `foo-1.2.3rc1`, so a `1.2.3` probe must not fire on either.
pub(crate) fn body_lists_version(body: &str, normalized_name: &str, version: &str) -> bool {
    body.split(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
        .filter_map(distribution_name_version)
        .any(|(name, ver)| ver == version && normalize_project_name(&name) == normalized_name)
}

/// Parse a distribution filename token into its `(name, version)`, or `None`
/// when the token is not a wheel/sdist filename.
///
/// PEP 427 escapes the distribution name so it carries no `-`, hence the
/// first `-` separates name from version for a wheel
/// (`foo_bar-1.2.3-py3-none-any.whl`) and the last `-` before the extension
/// does for an sdist (`foo_bar-1.2.3.tar.gz`).
fn distribution_name_version(token: &str) -> Option<(String, String)> {
    // A simple-index href is a path with an optional `#sha256=…` fragment
    // (`/simple/foo/foo-1.2.3-…whl#sha256=…`); reduce it to the bare filename
    // before parsing the escaped name has no `/`.
    let token = token.rsplit('/').next().unwrap_or(token);
    let token = token.split('#').next().unwrap_or(token);
    if let Some(stem) = token.strip_suffix(".whl") {
        let mut parts = stem.splitn(3, '-');
        let name = parts.next().filter(|s| !s.is_empty())?;
        let version = parts.next().filter(|s| !s.is_empty())?;
        return Some((name.to_string(), version.to_string()));
    }
    for ext in [".tar.gz", ".tar.bz2", ".tar.xz", ".zip"] {
        if let Some(stem) = token.strip_suffix(ext) {
            let (name, version) = stem.rsplit_once('-')?;
            if name.is_empty() || version.is_empty() {
                return None;
            }
            return Some((name.to_string(), version.to_string()));
        }
    }
    None
}

/// Config-time platform-tag collision check: two selected binaries that build
/// the SAME target triple derive the SAME wheel platform tag and — because a
/// wheel filename carries the PROJECT name, not the crate name — collide on
/// one identical `.whl`. The publish-time `seen_tags` bail catches this only
/// once binary bytes exist; this surfaces the likely collision at preflight
/// from config-derivable build targets so a multi-binary workspace is told to
/// narrow per-entry `ids:` before the run reaches the Manager group.
///
/// A Warning, not a Blocker: preflight cannot read each binary's glibc floor,
/// so two gnu binaries on one triple with different floors would tag distinct
/// `manylinux` versions and NOT collide — the run-path bail stays the hard
/// gate.
fn platform_tag_collision_check(ctx: &Context, cfg: &PypiConfig) -> anodizer_core::PreflightCheck {
    use anodizer_core::PreflightCheck;
    use std::collections::BTreeMap;

    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in ctx.config.crate_universe() {
        let selected = match cfg.ids.as_ref() {
            Some(ids) => ids.iter().any(|id| id == &c.name),
            None => true,
        };
        if !selected {
            continue;
        }
        for triple in crate_build_targets(ctx, c) {
            owners.entry(triple).or_default().push(c.name.clone());
        }
    }
    let mut collisions: Vec<String> = owners
        .into_iter()
        .filter(|(_, o)| o.len() >= 2)
        .map(|(t, mut o)| {
            o.dedup();
            format!("{t} (crates: {})", o.join(", "))
        })
        .collect();
    if collisions.is_empty() {
        return PreflightCheck::Pass;
    }
    collisions.sort();
    PreflightCheck::Warning(format!(
        "pypi: multiple selected binaries build the same target triple(s) [{}] — each \
         derives the same wheel platform tag and would collide on one filename; narrow \
         this entry's `ids:` so it publishes one binary per platform",
        collisions.join("; ")
    ))
}

/// Distinct-per-build target triples a crate would produce: each build's
/// explicit `targets` (or `effective_default_targets` when it inherits),
/// skipping builds whose `skip:` statically renders truthy. Over-collecting
/// is safe — a spurious hit is only a Warning.
fn crate_build_targets(ctx: &Context, c: &anodizer_core::config::CrateConfig) -> Vec<String> {
    let Some(builds) = c.builds.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for b in builds {
        let off = b
            .skip
            .as_ref()
            .map(|s| {
                s.try_evaluates_to_true(|t| ctx.render_template(t))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if off {
            continue;
        }
        match b.targets.as_ref().filter(|t| !t.is_empty()) {
            Some(t) => out.extend(t.iter().cloned()),
            None => out.extend(ctx.config.effective_default_targets()),
        }
    }
    out
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

    /// Per active entry: the upload token (a templated `cfg.token`'s env
    /// refs, else the `PYPI_TOKEN` / `MATURIN_PYPI_TOKEN` any-of fallback
    /// ladder), plus the `maturin` CLI when `sdist: true`.
    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let active: Vec<_> = ctx
            .config
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
            .collect();
        let mut out = Vec::new();
        if active.iter().any(|e| e.sdist) {
            out.push(anodizer_core::EnvRequirement::Tool {
                name: "maturin".to_string(),
            });
        }
        for entry in &active {
            match entry.token.as_deref().filter(|t| !t.is_empty()) {
                // Templated token: require its env refs (a literal declares
                // nothing — the credential is inline).
                Some(_) => out.extend(crate::publisher_helpers::secret_requirement(
                    entry.token.as_deref(),
                    TOKEN_ENV_VARS[0],
                )),
                // No configured token: either fallback env var satisfies the
                // run path's ladder.
                None => out.push(anodizer_core::EnvRequirement::EnvAnyOf {
                    vars: TOKEN_ENV_VARS.iter().map(|s| s.to_string()).collect(),
                }),
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
