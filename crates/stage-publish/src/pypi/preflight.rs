//! PyPI pre-publish and rollback index probes: repository-to-probe-URL
//! derivation, simple-index and JSON-API version checks, distribution-filename
//! parsing, and the config-time platform-tag collision check.

use std::time::Duration;

use anodizer_core::config::PypiConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::pep::{normalize_project_name, semver_to_pep440};

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
pub(crate) fn simple_index_lists_version(url: &str, normalized_name: &str, version: &str) -> bool {
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
pub(crate) fn platform_tag_collision_check(
    ctx: &Context,
    cfg: &PypiConfig,
) -> anodizer_core::PreflightCheck {
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
        for triple in crate::publisher_helpers::crate_build_targets(ctx, c) {
            // Honour the `targets:` allowlist: a triple filtered out never
            // becomes a wheel, so it cannot collide — a config that drops the
            // colliding gnu-windows build no longer warns.
            if !crate::publisher_helpers::target_in_allowlist(cfg.targets.as_ref(), &triple) {
                continue;
            }
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
