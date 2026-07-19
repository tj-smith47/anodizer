//! NPM publish auth and registry probes: token/OIDC credential resolution,
//! the per-package auth decision, and package/version/dist-tag existence probes.

use std::path::Path;
use std::process::Command;

use anodizer_core::config::{NpmAuthMode, NpmConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Result, bail};

use super::manifest::{DEFAULT_TAG, token_env_var};

/// Probe the registry for an existing `<name>@<version>` publication via
/// `npm view`.
///
/// Returns `Ok(true)` when the version is already published, `Ok(false)` only
/// on a definitive `E404` (the package/version genuinely does not exist).
///
/// Fail-closed on an inconclusive probe: a spawn failure or any non-404 error
/// shape (registry 5xx, auth failure, network glitch) surfaces an `Err`
/// rather than `Ok(false)`. An `npm publish` is irreversible after npm's 72h
/// unpublish window, so a probe that *cannot prove* the version is absent must
/// not green-light the publish — assuming "not published" on an outage would
/// re-push over an existing version (or double-ship) the moment the registry
/// recovers. The caller aborts this package's publish and records the failure
/// for the operator instead.
pub(crate) fn version_already_published(
    name: &str,
    version: &str,
    cfg_dir: &Path,
    registry: &str,
    log: &StageLogger,
) -> Result<bool> {
    let mut cmd = Command::new("npm");
    cmd.arg("view")
        .arg(format!("{}@{}", name, version))
        .arg("version")
        .arg("--registry")
        .arg(registry)
        .arg("--userconfig")
        .arg(cfg_dir.join(".npmrc"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            log.warn(&format!(
                "could not probe npm for '{}@{}' on {} (spawn failed: {}); \
                 refusing to publish blind — fix the npm CLI and retry",
                name, version, registry, e
            ));
            bail!(
                "npm: idempotency probe for '{}@{}' failed to spawn npm view",
                name,
                version
            );
        }
    };
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Ok(!stdout.trim().is_empty());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("E404") {
        return Ok(false);
    }
    log.warn(&format!(
        "npm idempotency probe for '{}@{}' on {} was inconclusive (not a 404): {}; \
         refusing to publish blind to a 72h-irreversible registry — retry once the \
         registry is healthy",
        name,
        version,
        registry,
        anodizer_core::redact::redact_bearer_tokens(stderr.trim())
    ));
    bail!(
        "npm: idempotency probe for '{}@{}' returned an inconclusive non-404 error",
        name,
        version
    );
}

/// Resolve the auth token: `cfg.token` (templated) precedence, then the
/// `NPM_TOKEN` env var. Empty when both are unset — the caller surfaces a
/// clear "missing token" error.
pub(crate) fn resolve_token(ctx: &Context, cfg: &NpmConfig) -> Result<String> {
    // The shared ladder filters empties at every rung: an exported-but-blank
    // `NPM_TOKEN` (GitHub Actions' shape for a missing secret) resolves to
    // absent rather than `""`, closing the gap `unwrap_or_default()` left.
    crate::publisher_helpers::resolve_token_with_ladder(
        ctx,
        cfg.token.as_deref(),
        "npm: render token template",
        &[token_env_var(cfg)],
    )
}

/// The two GitHub Actions OIDC request variables npm's Trusted Publishing
/// exchange consumes. Both must be present for an OIDC context to exist — the
/// URL is the token-mint endpoint, the token authorizes the mint request.
pub(crate) const OIDC_ENV_VARS: [&str; 2] = [
    "ACTIONS_ID_TOKEN_REQUEST_URL",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
];

/// Resolved npm publish credential. Exactly one variant authorizes a publish;
/// there is no anonymous variant by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NpmAuth {
    /// A long-lived registry token (`NPM_TOKEN` / `cfg.token`). Written as
    /// `_authToken` into the per-run `.npmrc`.
    Token(String),
    /// A GitHub Actions OIDC context (Trusted Publishing). Carries the
    /// `ACTIONS_ID_TOKEN_REQUEST_*` pairs to thread into the `npm publish`
    /// subprocess so the npm CLI mints a short-lived credential itself; the
    /// `.npmrc` carries no token line.
    Oidc(Vec<(String, String)>),
}

/// Snapshot the GitHub Actions OIDC request env when BOTH variables are present
/// and non-empty, returning every entry to thread into the publish subprocess.
/// Returns `None` (no OIDC context) when either variable is missing/empty.
fn resolve_oidc_env(ctx: &Context) -> Option<Vec<(String, String)>> {
    let env = ctx.env_source();
    let mut out = Vec::with_capacity(OIDC_ENV_VARS.len());
    for name in OIDC_ENV_VARS {
        let val = env.var(name).filter(|v| !v.is_empty())?;
        out.push((name.to_string(), val));
    }
    Some(out)
}

/// Whether a package already exists on the registry, used to drive per-package
/// auth selection in [`NpmAuthMode::Auto`]. `Unknown` is returned when the
/// existence probe could not reach a verdict (network error) — the decision
/// then prefers the safe path rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageExistence {
    /// Registry returned 200 — the package name is already published.
    Exists,
    /// Registry returned 404 — the package name is brand new.
    New,
    /// The probe failed (network/registry error) — existence is undetermined.
    Unknown,
}

/// The credential a per-package auth decision selects, as a pure outcome that
/// carries no secret material (the caller materializes the actual
/// [`NpmAuth`] from it). `FailNewNeedsToken` and `ErrorNoAuth` are terminal —
/// the package cannot be published with the inputs given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthDecision {
    /// Authenticate with the token.
    Token,
    /// Authenticate with OIDC (Trusted Publishing).
    Oidc,
    /// New package + OIDC-only context + no token: Trusted Publishing cannot
    /// create a non-existent package, so the initial publish needs a token.
    FailNewNeedsToken,
    /// No credential is available at all.
    ErrorNoAuth,
}

/// Decide a single package's publish credential from the four facts that govern
/// it: the configured [`NpmAuthMode`], whether the package already exists, and
/// whether an OIDC context / a token are available. Pure — no I/O, no secrets —
/// so the full decision matrix is unit-testable in isolation.
///
/// `auto` semantics (per package):
///
/// | exists?  | OIDC? | token? | decision           |
/// |----------|-------|--------|--------------------|
/// | new      | any   | yes    | `Token`            |
/// | new      | yes   | no     | `FailNewNeedsToken`|
/// | new      | no    | no     | `ErrorNoAuth`      |
/// | exists   | yes   | any    | `Oidc`             |
/// | exists   | no    | yes    | `Token`            |
/// | exists   | no    | no     | `ErrorNoAuth`      |
/// | unknown  | any   | yes    | `Token` (safe)     |
/// | unknown  | yes   | no     | `Oidc` (best effort)|
/// | unknown  | no    | no     | `ErrorNoAuth`      |
///
/// `token` mode forces [`AuthDecision::Token`] (or `ErrorNoAuth` if no token);
/// `oidc` mode forces [`AuthDecision::Oidc`] (or `ErrorNoAuth` if no OIDC
/// context) — strict Trusted-Publishing-only, no token fallback.
pub(crate) fn decide_auth(
    mode: NpmAuthMode,
    existence: PackageExistence,
    oidc_available: bool,
    token_available: bool,
) -> AuthDecision {
    match mode {
        NpmAuthMode::Token => {
            if token_available {
                AuthDecision::Token
            } else {
                AuthDecision::ErrorNoAuth
            }
        }
        NpmAuthMode::Oidc => {
            if oidc_available {
                AuthDecision::Oidc
            } else {
                AuthDecision::ErrorNoAuth
            }
        }
        NpmAuthMode::Auto => match existence {
            PackageExistence::New => {
                if token_available {
                    AuthDecision::Token
                } else if oidc_available {
                    // Trusted Publishing cannot create a package that does not
                    // yet exist — surface a specific, fixable error.
                    AuthDecision::FailNewNeedsToken
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
            PackageExistence::Exists => {
                if oidc_available {
                    AuthDecision::Oidc
                } else if token_available {
                    AuthDecision::Token
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
            PackageExistence::Unknown => {
                if token_available {
                    // Safe path on an inconclusive probe: a token can publish
                    // whether the package exists or not.
                    AuthDecision::Token
                } else if oidc_available {
                    AuthDecision::Oidc
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
        },
    }
}

/// URL-encode an npm package name for a registry metadata GET: a scoped name's
/// single `/` becomes `%2F` (`@a/b` → `@a%2Fb`); all other characters in valid
/// npm names (lowercase, digits, `-._@`) are already URL-safe.
pub(crate) fn encode_package_path(name: &str) -> String {
    name.replace('/', "%2F")
}

/// Probe the registry for a package's *existence* (any version) via a metadata
/// GET to `<registry>/<url-encoded name>`. 200 → [`PackageExistence::Exists`],
/// 404 → [`PackageExistence::New`]; any transport error or other status →
/// [`PackageExistence::Unknown`] (the caller's `auto` decision then prefers the
/// safe path). This is distinct from [`version_already_published`], which
/// probes for one specific *version* to drive idempotent re-runs.
pub(crate) fn probe_package_existence(
    registry: &str,
    name: &str,
    log: &StageLogger,
) -> PackageExistence {
    let base = registry.trim_end_matches('/');
    let url = format!("{}/{}", base, encode_package_path(name));
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(15)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "npm: could not build HTTP client to probe '{}' existence ({}); \
                 treating existence as unknown",
                name, e
            ));
            return PackageExistence::Unknown;
        }
    };
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status.as_u16() == 404 {
                PackageExistence::New
            } else if status.is_success() {
                PackageExistence::Exists
            } else {
                log.warn(&format!(
                    "npm: existence probe for '{}' returned HTTP {} (inconclusive); \
                     treating existence as unknown",
                    name, status
                ));
                PackageExistence::Unknown
            }
        }
        Err(e) => {
            log.warn(&format!(
                "npm: existence probe for '{}' failed ({}); treating existence as unknown",
                name, e
            ));
            PackageExistence::Unknown
        }
    }
}

/// Probe the registry for a package's current `latest` dist-tag via a metadata
/// GET to `<registry>/<url-encoded name>`, reading `.dist-tags.latest`. Returns
/// `None` on a 404 (brand-new package — nothing to regress), any transport /
/// decode error, or an absent tag. Every `None` path FAILS OPEN (the caller
/// keeps the configured tag): a missing signal must never block a legitimate
/// publish.
pub(crate) fn probe_dist_tag_latest(
    registry: &str,
    name: &str,
    log: &StageLogger,
) -> Option<String> {
    let base = registry.trim_end_matches('/');
    let url = format!("{}/{}", base, encode_package_path(name));
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(15)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "npm: could not build HTTP client to probe '{}' latest dist-tag ({}); \
                 leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => {
            log.warn(&format!(
                "npm: latest-tag probe for '{}' failed ({}); leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    // 404 = brand-new package (no `latest` to regress); any other non-2xx is
    // inconclusive. Both fail open.
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            log.warn(&format!(
                "npm: could not decode metadata for '{}' ({}); leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    body.get("dist-tags")
        .and_then(|d| d.get("latest"))
        .and_then(|l| l.as_str())
        .map(str::to_string)
}

/// Guard the mutable `latest` dist-tag against a version REGRESSION.
///
/// npm's `latest` is the tag `npm install <pkg>` (no version) resolves, and
/// publishing an OLDER version with `--tag latest` moves that pointer BACKWARD —
/// silently downgrading every default install. This bites a BACKFILL: completing
/// an interrupted older release after a newer one already published would drag
/// `latest` back to the older version.
///
/// When the configured tag is the default `latest` AND `publish_version` is
/// strictly LOWER than the registry's current `latest`, this returns an INERT
/// named dist-tag `release-<version>`: the version still publishes (versions are
/// immutable and always land), but the `latest` pointer is left on the newer
/// release. Every non-regressing case returns the configured tag unchanged — a
/// NON-default configured tag (the operator asked for an explicit tag),
/// `registry_latest == None` (fail-open), an equal/newer version, or a version
/// string that does not parse as semver.
///
/// The demoted tag is `release-<version>`, NOT the bare version: npm rejects any
/// `--tag` that parses as a semver range (`npm publish`:
/// `if (semver.validRange(tag)) throw "Tag name must not be a valid SemVer
/// range"`), and node-semver strips a leading `v`, so neither `0.19.0` nor
/// `v0.19.0` is a legal tag. The `release-` prefix makes the whole string
/// unparseable as a range while staying per-version, so sequential backfills
/// (0.19 → 0.20 → 0.21) never contend over one shared pointer.
pub(crate) fn guard_latest_regression(
    configured_tag: &str,
    publish_version: &str,
    registry_latest: Option<&str>,
) -> String {
    if configured_tag != DEFAULT_TAG {
        return configured_tag.to_string();
    }
    if let Some(current) = registry_latest {
        if let (Ok(pubv), Ok(cur)) = (
            anodizer_core::git::parse_semver(publish_version),
            anodizer_core::git::parse_semver(current),
        ) {
            if pubv < cur {
                return format!("release-{publish_version}");
            }
        }
    }
    configured_tag.to_string()
}

/// Apply [`guard_latest_regression`] against the live registry: when `dist_tag`
/// is the default `latest`, probe `package`'s current `latest` and demote to an
/// inert version-tag if `version` would regress it. A no-op (returns `dist_tag`
/// verbatim) for any explicit tag, so the network round-trip only happens when a
/// regression is actually possible. Emits a `status` line when it demotes so the
/// operator sees why `latest` was left untouched.
pub(crate) fn dist_tag_guarded_against_regression(
    dist_tag: &str,
    version: &str,
    registry: &str,
    package: &str,
    log: &StageLogger,
) -> String {
    if dist_tag != DEFAULT_TAG {
        return dist_tag.to_string();
    }
    let current = probe_dist_tag_latest(registry, package, log);
    let guarded = guard_latest_regression(dist_tag, version, current.as_deref());
    if guarded != dist_tag {
        log.status(&format!(
            "npm: publishing {}@{} under inert tag '{}' — registry 'latest' is {} (newer); \
             refusing to regress the default-install pointer",
            package,
            version,
            guarded,
            current.as_deref().unwrap_or("?")
        ));
    }
    guarded
}

/// Resolve the per-package publish credential for one package under the
/// configured [`NpmAuthMode`]: probe existence (only when `auto` needs it),
/// detect OIDC + token availability, run [`decide_auth`], then materialize the
/// actual [`NpmAuth`] (reading the token / OIDC env). Terminal decisions
/// hard-error with a specific, fixable message.
///
/// Returns the chosen [`NpmAuth`] alongside the resolved token string (empty
/// when no token is set) so the caller's OIDC→token fallback need not re-render
/// the token template.
pub(crate) fn resolve_auth_for_package(
    ctx: &Context,
    cfg: &NpmConfig,
    registry: &str,
    package: &str,
    log: &StageLogger,
) -> Result<(NpmAuth, String)> {
    let token = resolve_token(ctx, cfg)?;
    let token_available = !token.is_empty();
    let oidc = resolve_oidc_env(ctx);
    let oidc_available = oidc.is_some();

    // The existence probe only changes the `auto` verdict, and only when at
    // least one credential exists (with neither, the verdict is `ErrorNoAuth`
    // regardless of existence). Skip the network round-trip in the forced
    // `token` / `oidc` modes and when no credential is available.
    let existence = if cfg.auth == NpmAuthMode::Auto && (token_available || oidc_available) {
        probe_package_existence(registry, package, log)
    } else {
        PackageExistence::Unknown
    };

    match decide_auth(cfg.auth, existence, oidc_available, token_available) {
        AuthDecision::Token => Ok((NpmAuth::Token(token.clone()), token)),
        AuthDecision::Oidc => {
            let oidc = oidc.ok_or_else(|| {
                anyhow::anyhow!(
                    "npm: internal — OIDC chosen for '{}' without an OIDC env",
                    package
                )
            })?;
            Ok((NpmAuth::Oidc(oidc), token))
        }
        AuthDecision::FailNewNeedsToken => bail!(
            "npm: package '{}' does not exist and Trusted Publishing cannot create it — \
             set NPM_TOKEN (or cfg.token) for the initial publish, then switch the package \
             to Trusted Publishing once it exists",
            package
        ),
        AuthDecision::ErrorNoAuth => match cfg.auth {
            NpmAuthMode::Token => bail!(
                "npm: auth mode is `token` but no token is set for '{}' — set NPM_TOKEN \
                 (or cfg.token). Refusing to publish anonymously.",
                package
            ),
            NpmAuthMode::Oidc => bail!(
                "npm: auth mode is `oidc` but no OIDC context is present for '{}' — run under \
                 GitHub Actions with `id-token: write` (both ACTIONS_ID_TOKEN_REQUEST_URL and \
                 ACTIONS_ID_TOKEN_REQUEST_TOKEN must be set). Refusing to fall back to a token.",
                package
            ),
            NpmAuthMode::Auto => bail!(
                "npm: cannot authenticate '{}' — set NPM_TOKEN (or cfg.token), or run under \
                 GitHub Actions OIDC (id-token: write) with a Trusted Publisher configured on \
                 the registry. Refusing to publish anonymously.",
                package
            ),
        },
    }
}
