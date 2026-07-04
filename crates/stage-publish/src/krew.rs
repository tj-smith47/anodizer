use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::util::{self, OsArtifact};

// ---------------------------------------------------------------------------
// KrewManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating a Krew plugin manifest YAML.
pub(crate) struct KrewManifestParams<'a> {
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) homepage: &'a str,
    pub(crate) short_description: &'a str,
    pub(crate) description: &'a str,
    pub(crate) caveats: &'a str,
    /// `(os, arch, url, sha256, binary_name)` tuples for each platform.
    pub(crate) platforms: &'a [KrewPlatform],
}

/// A single platform entry in the Krew manifest.
#[derive(Default)]
pub(crate) struct KrewPlatform {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) bin: String,
    /// Per-platform `files:` extraction list (`from`/`to` pairs) selecting
    /// the binary plus any bundled LICENSE / README from the archive. Empty
    /// only when the artifact carried no layout/file metadata (legacy
    /// snapshots); the live path always populates at least the binary entry.
    pub(crate) files: Vec<KrewFileEntry>,
}

/// A single `files:` extraction entry in a krew platform.
///
/// `from` is the path *inside the downloaded archive* (carrying the
/// `wrap_in_directory` prefix for nested layouts); `to` is the destination
/// relative to the plugin's install dir. Real krew plugins (ctx/ns/tree/
/// access-matrix) emit `to: "."` to flatten the binary + LICENSE to the
/// install root, which is why `bin:` references the flat binary name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KrewFileEntry {
    pub(crate) from: String,
    pub(crate) to: String,
}

// ---------------------------------------------------------------------------
// Serde structs for Krew YAML manifest
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewManifestYaml {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: KrewMetadata,
    spec: KrewSpec,
}

#[derive(Serialize)]
struct KrewMetadata {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSpec {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    short_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caveats: Option<String>,
    platforms: Vec<KrewPlatformYaml>,
}

#[derive(Serialize)]
struct KrewPlatformYaml {
    selector: KrewSelector,
    uri: String,
    sha256: String,
    bin: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<KrewFileYaml>,
}

#[derive(Serialize)]
struct KrewFileYaml {
    from: String,
    to: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSelector {
    match_labels: KrewMatchLabels,
}

#[derive(Serialize)]
struct KrewMatchLabels {
    os: String,
    arch: String,
}

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Generate a Krew plugin manifest YAML string.
///
/// Uses `serde_yaml_ng` for proper YAML serialization with correct escaping
/// of special characters. The `description` and `caveats` fields use YAML
/// block scalar style (literal `|`) when present, achieved via post-processing.
pub(crate) fn generate_manifest(params: &KrewManifestParams<'_>) -> Result<String> {
    let mut platforms: Vec<KrewPlatformYaml> = params
        .platforms
        .iter()
        .map(|p| KrewPlatformYaml {
            selector: KrewSelector {
                match_labels: KrewMatchLabels {
                    os: p.os.clone(),
                    arch: krew_arch(&p.arch).to_string(),
                },
            },
            uri: p.url.clone(),
            sha256: p.sha256.clone(),
            bin: p.bin.clone(),
            files: p
                .files
                .iter()
                .map(|f| KrewFileYaml {
                    from: f.from.clone(),
                    to: f.to.clone(),
                })
                .collect(),
        })
        .collect();

    // sort platforms by URI descending.
    platforms.sort_by(|a, b| b.uri.cmp(&a.uri));

    let manifest = KrewManifestYaml {
        api_version: "krew.googlecontainertools.github.com/v1alpha2".to_string(),
        kind: "Plugin".to_string(),
        metadata: KrewMetadata {
            name: params.name.to_string(),
        },
        spec: KrewSpec {
            version: format!("v{}", params.version),
            homepage: if params.homepage.is_empty() {
                None
            } else {
                Some(params.homepage.to_string())
            },
            short_description: params.short_description.to_string(),
            description: if params.description.is_empty() {
                None
            } else {
                Some(params.description.to_string())
            },
            caveats: if params.caveats.is_empty() {
                None
            } else {
                Some(params.caveats.to_string())
            },
            platforms,
        },
    };

    let yaml = serde_yaml_ng::to_string(&manifest).context("krew: serialize manifest")?;

    Ok(format!(
        "# This file was generated by anodizer. DO NOT EDIT.\n{}",
        yaml
    ))
}

/// Resolve the effective krew plugin name: the `krew.name` override when set,
/// else the crate name, rendered through the template engine.
///
/// This is the single source of truth shared by the manifest `metadata.name`,
/// the `plugins/<name>.yaml` file basename, and the webhook `pluginName`.
/// krew-index CI rejects a plugin whose `metadata.name` disagrees with the
/// manifest filename, so these three must never drift apart.
fn resolve_plugin_name(
    name_override: Option<&str>,
    crate_name: &str,
    render: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    let raw = name_override.unwrap_or(crate_name);
    render(raw).with_context(|| format!("krew: render plugin name template for '{}'", crate_name))
}

/// Map the internal arch names to Krew's expected labels.
///
/// This is a publisher-specific mapping layer on top of the generic
/// `infer_arch` in `util.rs`. The util layer produces canonical short
/// forms (`"amd64"`, `"arm64"`), and this function translates them
/// to whatever Krew expects. Today the mapping is a no-op for the
/// common cases, but keeping a separate layer allows adapting to
/// future Krew label changes without touching the shared inference.
fn krew_arch(arch: &str) -> &str {
    match arch {
        "amd64" | "x86_64" => "amd64",
        "arm64" | "aarch64" => "arm64",
        other => other,
    }
}

/// The krew-index review convention for `shortDescription` length. The
/// krew-index CI hints at taglines no longer than ~50 characters (exemplars:
/// ctx=35, ns=33); longer ones get flagged in human review. anodizer warns
/// rather than truncating — silently dropping the tail of a tagline risks
/// losing meaning, and the maintainer is best placed to shorten it.
const KREW_SHORT_DESCRIPTION_MAX: usize = 50;

/// Warn (loudly, naming the field + the actual length) when a rendered
/// `shortDescription` exceeds the krew-index norm, so the user can shorten it
/// before krew-index review flags the submission. Counts Unicode scalar values,
/// not bytes, to match how a human reads the tagline.
fn warn_if_short_description_too_long(
    short_description: &str,
    crate_name: &str,
    log: &StageLogger,
) {
    let len = short_description.chars().count();
    if len > KREW_SHORT_DESCRIPTION_MAX {
        log.warn(&format!(
            "krew: shortDescription for '{}' is {} characters (krew-index review \
             flags taglines longer than ~{}). Shorten `krew.short_description` to \
             keep the submission within the norm: \"{}\"",
            crate_name, len, KREW_SHORT_DESCRIPTION_MAX, short_description
        ));
    }
}

/// Map the internal OS names to Krew's expected labels.
///
/// See `krew_arch` for the rationale behind keeping a separate mapping
/// layer on top of `infer_os` in `util.rs`.
fn krew_os(os: &str) -> &str {
    match os {
        "darwin" | "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// krew-release-bot mode selection
// ---------------------------------------------------------------------------
//
// A plugin's first appearance in `kubernetes-sigs/krew-index` requires a
// human-reviewed PR; subsequent version bumps are mechanical. The krew
// maintainers run a hosted webhook (`krew-release-bot`) that performs the
// fork + version-bump PR server-side, under the bot's own GitHub account,
// for any plugin already in the index. anodizer drives that webhook
// directly so a release is self-contained — no separate GitHub-Actions
// workflow step is required.
//
// In `auto` mode the deciding signal is whether the plugin already
// exists in krew-index:
//   - Plugin NOT in index → `PrDirect`: anodizer clones a fork, writes
//     `plugins/<name>.yaml`, commits, and opens the initial PR against
//     `kubernetes-sigs/krew-index`. A human reviews + merges it.
//   - Plugin IS in index → `BotWebhook`: anodizer POSTs a `ReleaseRequest`
//     (the fully-rendered manifest plus the release tag) to the hosted
//     webhook, which opens the version-bump PR on the plugin's behalf.
//     No fork, no token, no workflow.
//
// The membership probe is a GET against the GitHub contents API:
//   `api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/<name>.yaml`
// → 200 means published; 404 means not yet. Any other status
// (rate-limit, 5xx) is indeterminate: `auto` mode then HARD-ERRORS
// rather than guessing, because a transient blip must never route a
// plugin already in the index into a fork PR (krew maintainers reject
// mechanical version bumps submitted as fork PRs). The probe is
// authenticated whenever a token is in context — the same token used
// for the GitHub release — which raises the rate limit from 60/hr (anon)
// to 5,000/hr and eliminates almost all indeterminate results. Set the
// krew `mode` config field to `bot` or `pr-direct` to skip the probe
// entirely.

/// The two flows the krew publisher dispatches between, after the
/// user-facing [`KrewMode`](anodizer_core::config::KrewMode) config knob
/// (and, in `auto`, the membership probe) have been resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KrewFlow {
    /// Initial-submission flow — plugin isn't in krew-index yet.
    /// Behaviour: clone fork, write `plugins/<name>.yaml`, commit, PR
    /// against `kubernetes-sigs/krew-index`.
    PrDirect,
    /// Version-update flow — plugin IS in krew-index. Behaviour: POST a
    /// `ReleaseRequest` to the hosted krew-release-bot webhook, which
    /// opens the krew-index PR server-side. Self-contained: no fork, no
    /// token, no GitHub-Actions workflow step.
    BotWebhook,
}

/// Resolve the krew submission flow from the configured `mode` and (in
/// `auto`) a krew-index membership probe.
///
/// - `Bot` → [`KrewFlow::BotWebhook`] (probe skipped).
/// - `PrDirect` → [`KrewFlow::PrDirect`] (probe skipped).
/// - `Auto` → probe membership: definitively in-index →
///   `BotWebhook`; definitively absent → `PrDirect`; indeterminate
///   (rate-limit / network / unexpected status) → `Err`, so the caller
///   fails loudly instead of guessing the maintainer-hostile path.
///
/// `token` is the GitHub token resolved from the krew repository config
/// (else the release token); passing it authenticates the probe.
fn detect_krew_flow(
    mode: anodizer_core::config::KrewMode,
    plugin_name: &str,
    token: Option<&str>,
) -> Result<KrewFlow> {
    use anodizer_core::config::KrewMode;
    match mode {
        KrewMode::Bot => Ok(KrewFlow::BotWebhook),
        KrewMode::PrDirect => Ok(KrewFlow::PrDirect),
        KrewMode::Auto => map_auto_probe(plugin_name, is_plugin_in_krew_index(plugin_name, token)),
    }
}

/// Pure dispatch for `auto` mode from a membership-probe result.
/// `Some(true)` → webhook flow; `Some(false)` → fork PR; `None`
/// (indeterminate) → loud error with an actionable hint, never a silent
/// fallback into the maintainer-hostile fork-PR path.
fn map_auto_probe(plugin_name: &str, in_index: Option<bool>) -> Result<KrewFlow> {
    match in_index {
        Some(true) => Ok(KrewFlow::BotWebhook),
        Some(false) => Ok(KrewFlow::PrDirect),
        None => anyhow::bail!(
            "krew: could not determine krew-index membership for plugin '{}' \
             (the contents-API probe failed — likely a rate-limit or network \
             error). Refusing to guess: an existing plugin wrongly routed to a \
             fork PR is rejected by krew maintainers. Retry the release, ensure \
             a GitHub token is available ({}) \
             to raise the API rate limit, or set the krew `mode` field \
             explicitly to `bot` or `pr-direct`.",
            plugin_name,
            anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / ")
        ),
    }
}

/// HTTP probe: does `kubernetes-sigs/krew-index/plugins/<name>.yaml` exist?
/// Returns:
///   - `Some(true)` → 200 OK, the plugin is published.
///   - `Some(false)` → 404 Not Found, the plugin is not yet published.
///   - `None` → network error, rate-limit, or unexpected status. Caller
///     treats this as indeterminate and (in `auto` mode) hard-errors.
///
/// `token` is optional — anodizer's GitHub PATs are scoped enough that
/// passing one raises the rate limit from 60/hr (anon) to 5,000/hr
/// (authenticated). The caller passes the release token so the probe is
/// authenticated in CI, which is what makes the `None`→hard-error path
/// rare in practice.
fn is_plugin_in_krew_index(plugin_name: &str, token: Option<&str>) -> Option<bool> {
    // Deliberately NOT routed through `core::http::github_api_base`: the
    // upstream kubernetes-sigs/krew-index lives on public github.com
    // regardless of the user's forge configuration or API-base override.
    let url = format!(
        "https://api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/{}.yaml",
        plugin_name
    );
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(10)).ok()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().ok()?;
    let status = resp.status();
    if status.is_success() {
        return Some(true);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Some(false);
    }
    // 403 (rate limited / token denied), 5xx (GitHub flaking) surface as
    // `None` (indeterminate). The probe runs only in `auto` mode, where an
    // indeterminate result is a hard error rather than a guess — an existing
    // plugin wrongly routed to a fork PR is rejected by krew maintainers.
    // Explicit `bot` / `pr-direct` modes never reach this probe.
    None
}

// ---------------------------------------------------------------------------
// krew-release-bot webhook submission
// ---------------------------------------------------------------------------

/// Default hosted krew-release-bot webhook endpoint. The bot forks
/// krew-index and opens the version-bump PR server-side under its own
/// GitHub account, so anodizer sends no token.
const DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL: &str =
    "https://krew-release-bot.rajatjindal.com/github-action-webhook";

/// Resolve the effective webhook URL: the `KREW_RELEASE_BOT_WEBHOOK_URL`
/// env var (trimmed, empty treated as unset) else
/// [`DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL`]. Mirrors the bot client's own
/// `getWebhookURL()` precedence so a self-hosted deployment is reachable
/// the same way.
fn resolve_webhook_url(env: &dyn anodizer_core::env_source::EnvSource) -> String {
    env.var("KREW_RELEASE_BOT_WEBHOOK_URL")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL.to_string())
}

/// The JSON body POSTed to the krew-release-bot webhook.
///
/// Field names and shapes mirror the bot's server-side `ReleaseRequest`
/// struct: `processed_template` is the fully-rendered manifest bytes
/// (the server's Go decoder expects a base64 string for its `[]byte`
/// field, which serde produces from a `Vec<u8>` only via an explicit
/// encoder — handled at the call site). The server validates the
/// manifest and commits these bytes to its krew-index fork verbatim
/// (it does not fetch release assets or recompute shas), so
/// `processed_template` already carries the final sha256 digests.
#[derive(Debug, Serialize)]
struct KrewReleaseRequest {
    #[serde(rename = "tagName")]
    tag_name: String,
    #[serde(rename = "pluginName")]
    plugin_name: String,
    #[serde(rename = "pluginOwner")]
    plugin_owner: String,
    #[serde(rename = "pluginRepo")]
    plugin_repo: String,
    #[serde(rename = "pluginReleaseActor")]
    plugin_release_actor: String,
    #[serde(rename = "templateFile")]
    template_file: String,
    /// Base64 of the rendered manifest bytes. The bot's `[]byte` JSON
    /// field decodes from a base64 string (Go's `encoding/json`
    /// convention), so the bytes are pre-encoded here.
    #[serde(rename = "processedTemplate")]
    processed_template: String,
}

impl KrewReleaseRequest {
    /// Build a `ReleaseRequest` from the resolved release coordinates and
    /// the fully-rendered manifest. `tag_name` is normalized to the
    /// `v<semver>` shape the krew-index manifest's `spec.version` carries.
    fn new(
        tag_name: &str,
        plugin_name: &str,
        plugin_owner: &str,
        plugin_repo: &str,
        plugin_release_actor: &str,
        rendered_manifest: &str,
    ) -> Self {
        use base64::Engine as _;
        Self {
            tag_name: tag_name.to_string(),
            plugin_name: plugin_name.to_string(),
            plugin_owner: plugin_owner.to_string(),
            plugin_repo: plugin_repo.to_string(),
            plugin_release_actor: plugin_release_actor.to_string(),
            template_file: ".krew.yaml".to_string(),
            processed_template: base64::engine::general_purpose::STANDARD
                .encode(rendered_manifest.as_bytes()),
        }
    }
}

/// Whether a non-200 webhook response body indicates the version/PR is
/// already submitted (an idempotent re-run), versus a genuine failure.
///
/// The bot server returns HTTP 500 for every failure path, wrapping the
/// underlying error message in the response body (`opening pr: <err>`).
/// Only two of those failure messages are benign re-runs of work the
/// previous submission already did. First, a PR for the same fork branch
/// already exists: GitHub's create-PR call fails with a 422 whose
/// message contains `pull request already exists`. Second, the manifest
/// is unchanged, so the commit step finds a clean tree and reports
/// `nothing to commit` / `clean working tree`.
///
/// The match is deliberately narrow — only these exact server phrases —
/// so a future genuine server error (a validation failure, an auth
/// error, an unexpected 5xx) is NOT silently swallowed as "already
/// submitted". Silently skipping a one-way publish is the worst failure
/// mode, so anything outside these phrases falls through to a loud
/// error.
fn webhook_body_is_already_submitted(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("pull request already exists")
        || lower.contains("nothing to commit")
        || lower.contains("clean working tree")
}

/// POST a [`KrewReleaseRequest`] to the krew-release-bot webhook and map
/// the response to a publish result.
///
/// A single attempt with a 30s timeout mirrors the bot's own client
/// (the PR-submit action runner). The retry helper is deliberately
/// NOT used here: the server returns HTTP 500 for every failure path,
/// including the benign "PR already exists" case, so a generic 5xx-retry
/// classifier would both burn the budget on an idempotent re-run and
/// flood the bot with duplicate submissions.
///
/// Outcome mapping:
///   - HTTP 200 → success; the body (`PR "<url>" submitted successfully`)
///     is logged.
///   - non-200 whose body matches [`webhook_body_is_already_submitted`] →
///     idempotent no-op success (the version was already submitted).
///   - any other non-200 / transport error → loud error. The release
///     must not silently skip krew.
fn submit_krew_release_webhook(
    webhook_url: &str,
    request: &KrewReleaseRequest,
    plugin_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<()> {
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        .context("krew: build webhook HTTP client")?;
    let body = serde_json::to_string(request).context("krew: serialize ReleaseRequest")?;

    let resp = client
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .with_context(|| format!("krew: POST to krew-release-bot webhook {}", webhook_url))?;

    let status = resp.status();
    let resp_body = anodizer_core::http::body_of_blocking(resp);

    if status.is_success() {
        log.status(&format!(
            "submitted krew plugin {} v{} via bot-webhook to {} ({})",
            plugin_name,
            version,
            webhook_url,
            resp_body.trim()
        ));
        return Ok(());
    }

    if webhook_body_is_already_submitted(&resp_body) {
        log.status(&format!(
            "krew plugin {} v{} already submitted upstream — treating as \
             idempotent no-op (webhook HTTP {})",
            plugin_name, version, status
        ));
        return Ok(());
    }

    anyhow::bail!(
        "krew: krew-release-bot webhook {} returned HTTP {} for plugin '{}' v{}: {}",
        webhook_url,
        status,
        plugin_name,
        version,
        resp_body.trim()
    )
}

/// Convert `OsArtifact`s into `KrewPlatform`s.
///
/// When an artifact has arch "all", it is expanded into platform entries
/// for both amd64 and arm64.
///
/// `bin:` resolution per platform:
/// 1. Use the artifact's in-archive binary name when known
///    (`OsArtifact.binary`, populated from `extra_binaries[0]` for archives
///    or the `binary` metadata for uploadable binaries).
/// 2. Fall back to `default_binary_name` (the crate name) when the artifact
///    didn't carry a binary name.
/// 3. Append `.exe` for Windows targets when the resolved name doesn't
///    already end in `.exe`. Krew takes `bin:` literally — it does NOT
///    add `.exe` itself — so a Windows entry without the suffix fails to
///    install (krew validator: "source binary cannot be found in extracted
///    archive"). The `.exe` suffix is produced naturally because the
///    builder appends it to `binary.Name`; anodizer's archive metadata
///    stores the suffix-less name, so we normalize here.
fn artifacts_to_platforms(
    artifacts: &[OsArtifact],
    default_binary_name: &str,
) -> Vec<KrewPlatform> {
    fn resolve_bin(a: &OsArtifact, default: &str, target_os: &str) -> String {
        let base = a.binary.clone().unwrap_or_else(|| default.to_string());
        if target_os == "windows" && !base.to_ascii_lowercase().ends_with(".exe") {
            format!("{}.exe", base)
        } else {
            base
        }
    }

    let mut platforms = Vec::new();
    for a in artifacts {
        let os = krew_os(&a.os).to_string();
        let bin = resolve_bin(a, default_binary_name, &os);
        let files = derive_krew_files(a, &bin);
        if a.arch == "all" {
            // Expand "all" into amd64 + arm64 entries
            for expanded_arch in &["amd64", "arm64"] {
                platforms.push(KrewPlatform {
                    os: os.clone(),
                    arch: expanded_arch.to_string(),
                    url: a.url.clone(),
                    sha256: a.sha256.clone(),
                    bin: bin.clone(),
                    files: files.clone(),
                });
            }
        } else {
            platforms.push(KrewPlatform {
                arch: krew_arch(&a.arch).to_string(),
                url: a.url.clone(),
                sha256: a.sha256.clone(),
                bin: bin.clone(),
                files: files.clone(),
                os,
            });
        }
    }
    platforms
}

/// Derive the per-platform `files:` extraction list for a krew platform entry.
///
/// Mirrors how every real krew-index plugin (ctx / ns / tree / access-matrix)
/// shapes `files:` — a `from`/`to` pair per file to lift out of the downloaded
/// archive, with `to: "."` flattening everything to the plugin install root
/// (which is why `bin:` references the flat binary name):
///
/// 1. **Binary** — always emitted. `from` is the binary's path *inside the
///    archive*: `<wrap_in_directory>/<bin>` for a nested archive, else `<bin>`.
///    Without this entry krew's default extractor can fail to find a nested
///    binary ("source binary cannot be found in extracted archive").
/// 2. **LICENSE** — emitted once when the archive bundles a `LICENSE*` file
///    (gated on `OsArtifact.archive_files`, the actual archive contents), with
///    `from` carrying its real in-archive path (wrap prefix included).
/// 3. **README** (`*.md`) — emitted for each bundled markdown doc, same gating.
///
/// `bin` is the already-resolved install-dir binary name (`.exe`-suffixed on
/// Windows). The `from` path re-derives the suffix-aware in-archive name from
/// it so the Windows `.exe` handling carries into the extraction list.
fn derive_krew_files(a: &OsArtifact, bin: &str) -> Vec<KrewFileEntry> {
    /// Join the `wrap_in_directory` prefix onto an in-archive file name,
    /// normalising to forward slashes (archive paths are always `/`-separated).
    fn in_archive_path(wrap: Option<&str>, name: &str) -> String {
        match wrap {
            Some(prefix) if !prefix.is_empty() => {
                format!("{}/{}", prefix.trim_end_matches('/'), name)
            }
            _ => name.to_string(),
        }
    }

    let wrap = a.wrap_in_directory.as_deref();
    let mut files = vec![KrewFileEntry {
        from: in_archive_path(wrap, bin),
        to: ".".to_string(),
    }];

    // LICENSE: include the first bundled LICENSE* file (krew flattens it to the
    // install root). Real plugins emit exactly one LICENSE entry; `archive_files`
    // is deterministically ordered (lowercase glob before uppercase) so the pick
    // is stable.
    let license_path = a
        .archive_files
        .iter()
        .find(|p| is_license(basename(p)))
        .cloned();
    if let Some(ref license) = license_path {
        files.push(KrewFileEntry {
            from: license.clone(),
            to: ".".to_string(),
        });
    }

    // README / markdown docs: include each bundled `*.md` (typically README.md),
    // but NOT the changelog (`CHANGELOG.md` is excluded from the krew install)
    // and NOT a `LICENSE.md` already selected above (it matches both the license
    // glob and `*.md`, which would otherwise duplicate the entry).
    for md in a.archive_files.iter().filter(|p| {
        let b = basename(p).to_ascii_lowercase();
        b.ends_with(".md") && !b.starts_with("changelog") && !is_license(basename(p))
    }) {
        files.push(KrewFileEntry {
            from: md.clone(),
            to: ".".to_string(),
        });
    }

    files
}

/// Whether an in-archive file basename is a LICENSE file (case-insensitive),
/// e.g. `LICENSE`, `license.txt`, `LICENSE.md`, `LICENSE-MIT`.
fn is_license(basename: &str) -> bool {
    basename.to_ascii_lowercase().starts_with("license")
}

/// The final path component of a `/`-separated in-archive path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// publish_to_krew
// ---------------------------------------------------------------------------

/// Per-crate outcome returned by [`publish_to_krew`].
///
/// `pushed` flags whether the run made a real upstream side effect
/// (the `PrDirect` flow's branch push + PR open). Drives the caller's
/// `any_pushed` gate that decides whether to populate rollback evidence.
/// The `BotWebhook` flow always leaves `pushed = false`: the
/// krew-release-bot server owns the krew-index PR, so anodizer has
/// nothing to roll back.
#[derive(Debug, Default, Clone)]
pub struct KrewPublishOutcome {
    /// `true` when the `PrDirect` flow pushed a branch + opened a PR.
    /// The caller's `any_pushed` gate checks this.
    pub pushed: bool,
}

impl KrewPublishOutcome {
    /// Convenience constructor for run paths that exit before reaching
    /// the webhook / push branches.
    fn skipped() -> Self {
        Self { pushed: false }
    }
}

/// Whether `crate_name` has at least one krew-eligible archive artifact under
/// `krew_cfg` in this run.
///
/// Routes through the same `find_all_platform_artifacts_with_variant` collector
/// the live publish uses (honoring the `ids` allow-list and the
/// amd64/arm microarchitecture-variant filters), so the eligibility predicate is
/// one source of truth: the live path errors when this would be `false` (no
/// archive to construct the manifest from), and the offline schema validator
/// skips the crate on the same signal. A single-target / sharded snapshot that
/// built no archive for this crate therefore yields `false` here rather than
/// tripping the publisher's "no archive artifacts" guard.
pub(crate) fn crate_has_krew_artifacts(
    ctx: &Context,
    crate_name: &str,
    krew_cfg: &anodizer_core::config::KrewConfig,
) -> Result<bool> {
    let ids_filter = krew_cfg.ids.as_deref();
    let amd64_variant = krew_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = krew_cfg.arm_variant.as_deref();
    let artifacts = util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        arm_variant,
    )?;
    Ok(!artifacts.is_empty())
}

/// Resolve a crate's krew config and render its plugin manifest in-memory, with
/// no clone, disk, or network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip`,
/// `skip_upload`, or a falsy `if` condition). Errors when the crate carries no
/// `krew` block, when a required narrative field (description / short
/// description) is unset, when an archive carries more than one binary (krew
/// allows exactly one per platform), when no eligible archive artifact exists,
/// or when a matched archive is missing its `sha256` metadata. The live publish
/// path and the offline schema validator both call this so the validated
/// document is byte-for-byte what a real publish would push.
pub(crate) fn render_krew_manifest_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "krew")?;
    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    // Honor `skip` first (template-aware), then the falsy-`if` gate, then
    // `skip_upload` — the same order and short-circuit the live publish applies,
    // so a skipped crate yields `None` (nothing to render or validate).
    if let Some(d) = krew_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("krew: render skip template for '{}'", crate_name))?;
        if off {
            return Ok(None);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        krew_cfg.if_condition.as_deref(),
        &format!("krew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        return Ok(None);
    }
    if util::should_skip_upload(krew_cfg.skip_upload.as_ref(), ctx, log, None)? {
        return Ok(None);
    }

    let version = ctx.version();

    // Validate required narrative fields before proceeding, falling back to
    // `metadata.description` when the krew config leaves them unset.
    let effective_description: Option<&str> = krew_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name));
    if effective_description.is_none_or(str::is_empty) {
        anyhow::bail!("krew: manifest description is not set for '{}'", crate_name);
    }
    // `short_description` is a krew-required tagline with no Cargo.toml
    // counterpart; fall back to the (possibly Cargo.toml-derived) description
    // so a plain Rust project does not hard-error on it.
    if krew_cfg
        .short_description
        .as_ref()
        .is_none_or(|s| s.is_empty())
        && effective_description.is_none_or(str::is_empty)
    {
        anyhow::bail!(
            "krew: manifest short_description is not set for '{}'",
            crate_name
        );
    }

    let description_raw = krew_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description = util::render_or_warn(ctx, log, "krew.description", description_raw)?;
    let short_description_raw = krew_cfg
        .short_description
        .as_deref()
        .or(effective_description)
        .unwrap_or(crate_name);
    let short_description =
        util::render_or_warn(ctx, log, "krew.short_description", short_description_raw)?;
    warn_if_short_description_too_long(&short_description, crate_name, log);
    // Derive GitHub slug (owner/repo) for the homepage fallback, consistent with
    // the homebrew publisher.
    let plugin_github = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| (gh.owner.clone(), gh.name.clone()));
    let github_slug = plugin_github
        .as_ref()
        .map(|(owner, name)| format!("{}/{}", owner, name));
    // The homepage fallback's final arm needs the krew-index repo owner; resolve
    // it the same way the live path does, but only for the fallback (no error
    // when the repository block is absent and another fallback already applies).
    // The live path requires the repository block before rendering, so on that
    // path the owner is always present and this matches its output. An empty /
    // absent owner (only reachable via the offline validator, which does not
    // require the block) drops the final arm rather than emit a degenerate
    // `https://github.com//crate` URL — never widen leniency past the live
    // path's guarantees.
    let repo_owner_fallback = crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
        .map(|(owner_raw, _)| util::render_or_warn(ctx, log, "krew.repository.owner", &owner_raw))
        .transpose()?
        .filter(|owner| !owner.is_empty());
    let homepage_raw = krew_cfg
        .homepage
        .clone()
        .or_else(|| ctx.config.meta_homepage_for(crate_name).map(str::to_string))
        .or_else(|| {
            github_slug
                .as_deref()
                .map(|slug| format!("https://github.com/{}", slug))
        })
        .or_else(|| {
            repo_owner_fallback
                .as_deref()
                .map(|owner| format!("https://github.com/{}/{}", owner, crate_name))
        })
        .unwrap_or_default();
    let homepage = ctx
        .render_template(&homepage_raw)
        .with_context(|| format!("krew: render homepage template for '{}'", crate_name))?;
    let caveats_raw = krew_cfg.caveats.clone().unwrap_or_default();
    let caveats = ctx
        .render_template(&caveats_raw)
        .with_context(|| format!("krew: render caveats template for '{}'", crate_name))?;

    // Find artifacts across all platforms, applying the IDs +
    // amd64_variant/arm_variant filters.
    let ids_filter = krew_cfg.ids.as_deref();
    let amd64_variant = krew_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = krew_cfg.arm_variant.as_deref();

    // Krew plugins support a single binary per archive. Walk the eligible
    // archives — through the SAME `ids` allow-list `find_all_platform_artifacts_with_variant`
    // applies (via the shared `filter_by_ids`), never a hand-rolled inline copy —
    // so an `ids`-excluded archive's binary count is not mistakenly enforced.
    let archives = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name);
    for archive in util::filter_by_ids(archives, ids_filter) {
        let binary_count = archive.extra_binaries().len();
        if binary_count != 1 {
            anyhow::bail!(
                "krew: only one binary per archive allowed, got {} on {:?}",
                binary_count,
                archive.name
            );
        }
    }

    let all_artifacts = util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        arm_variant,
    )?;

    let url_template = krew_cfg.url_template.as_deref();

    if all_artifacts.is_empty() {
        // An empty archive set is a hard error — a krew manifest with no real
        // artifacts is unusable (a placeholder URL produces 404s on install).
        anyhow::bail!(
            "krew: no archive artifacts found for '{}'. The krew publisher \
             needs at least one platform archive to construct the manifest. \
             Either add Windows/Linux/macOS targets for this crate or remove \
             the krew publisher config.",
            crate_name
        );
    }
    // krew's `addURIAndSha` validator rejects manifests whose
    // `spec.platforms[].sha256` is empty ("Hash validation failed"). Empty
    // sha256 metadata would silently produce an unusable plugin manifest.
    if let Some(empty) = all_artifacts.iter().find(|a| a.sha256.is_empty()) {
        anyhow::bail!(
            "krew: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated krew plugin \
             manifest would embed an empty `sha256:` field, which krew \
             rejects at install time. Check dist/artifacts.json for the \
             archive entry's metadata.sha256, and re-run `task release` from \
             a clean dist/ if the field is absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }
    let platforms = {
        let mut plats = artifacts_to_platforms(&all_artifacts, crate_name);
        if let Some(tmpl) = url_template {
            for p in &mut plats {
                p.url = util::render_url_template_with_ctx(
                    ctx, tmpl, crate_name, &version, &p.arch, &p.os,
                );
            }
        }
        plats
    };

    // Resolve the plugin name (honoring the `krew.name` override) so the
    // manifest `metadata.name` carries the same value the live path stamps onto
    // the published file basename and the webhook `pluginName`.
    let plugin_name_rendered = resolve_plugin_name(krew_cfg.name.as_deref(), crate_name, |t| {
        ctx.render_template(t)
    })?;
    let plugin_name = plugin_name_rendered.as_str();

    let manifest = generate_manifest(&KrewManifestParams {
        name: plugin_name,
        version: &version,
        homepage: &homepage,
        short_description: &short_description,
        description: &description,
        caveats: &caveats,
        platforms: &platforms,
    })?;

    Ok(Some(manifest))
}

pub fn publish_to_krew(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<KrewPublishOutcome> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "krew")?;

    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    // Honor `skip` first (template-aware), then `skip_upload`. `skip` lets
    // projects that aren't kubectl plugins keep a krew block in shared
    // config and turn it off without removing the surrounding
    // repository/short_description boilerplate.
    if let Some(d) = krew_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("krew: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "skipped krew config for '{}' — skip=true",
                crate_name
            ));
            return Ok(KrewPublishOutcome::skipped());
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        krew_cfg.if_condition.as_deref(),
        &format!("krew publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped krew for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(KrewPublishOutcome::skipped());
    }
    if util::should_skip_upload(
        krew_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("krew for '{crate_name}'")),
    )? {
        return Ok(KrewPublishOutcome::skipped());
    }

    // Resolve repository owner/name from `repository:` (RepositoryConfig).
    // Repository fields are template-rendered.
    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("krew: no repository config for '{}'", crate_name))?;
    let repo_owner = util::render_or_warn(ctx, log, "krew.repository.owner", &repo_owner_raw)?;
    let repo_name = util::render_or_warn(ctx, log, "krew.repository.name", &repo_name_raw)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit Krew plugin manifest for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(KrewPublishOutcome::skipped());
    }

    let version = ctx.version();

    // Render the plugin manifest via the same path the schema validator uses.
    // The skip / `if:` / skip_upload gates were already evaluated above; the
    // renderer re-checks them (returning None) but on this path always yields
    // Some. All field resolution, the one-binary-per-archive check, the
    // artifact collection, and the manifest serialization live in the shared
    // renderer so the validated document is byte-for-byte what is published.
    let Some(manifest) = render_krew_manifest_for_crate(ctx, crate_name, log)? else {
        return Ok(KrewPublishOutcome::skipped());
    };

    // The plugin's GitHub coordinates and the resolved plugin name are reused
    // below (webhook provenance, branch name, PR title). Recomputed here
    // (cheap, side-effect-free) because the renderer consumed them internally;
    // `resolve_plugin_name` is idempotent, so the value matches the manifest's
    // `metadata.name` exactly.
    let plugin_github = _crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| (gh.owner.clone(), gh.name.clone()));
    let plugin_name_rendered = resolve_plugin_name(krew_cfg.name.as_deref(), crate_name, |t| {
        ctx.render_template(t)
    })?;
    let plugin_name = plugin_name_rendered.as_str();

    // Clone the krew-index fork, write the plugin manifest, commit, push.
    let token =
        util::resolve_repo_token(ctx, krew_cfg.repository.as_ref(), Some("KREW_INDEX_TOKEN"));

    // A plugin already in krew-index takes the self-contained webhook
    // flow: anodizer POSTs the rendered manifest + tag to the hosted bot,
    // which opens the version-bump PR server-side. A plugin not yet in
    // the index takes the PR-direct flow below (clone fork → write
    // manifest → open the initial PR). In `auto` the choice comes from a
    // token-authenticated membership probe that hard-errors on an
    // indeterminate result; `mode: bot` / `mode: pr-direct` force the
    // flow and skip the probe.
    let mode = krew_cfg.mode.unwrap_or_default();
    let flow = detect_krew_flow(mode, plugin_name, token.as_deref())?;
    if flow == KrewFlow::BotWebhook {
        // The bot identifies the submission by the plugin's OWN GitHub
        // repo (owner/repo/tag), not the krew-index fork coordinates
        // resolved above. Require it: the server records these in the
        // PR's provenance, so missing coordinates would silently
        // mis-target the bot.
        let (plugin_owner, plugin_repo) = plugin_github.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "krew: plugin '{}' is in krew-index (webhook flow) but has no \
                 `release.github` owner/repo — the krew-release-bot webhook \
                 needs the plugin's GitHub repo to identify the submission",
                plugin_name
            )
        })?;
        // Actor should be a GitHub login. Prefer the CI-provided
        // GITHUB_ACTOR, then ANODIZER_GITHUB_ACTOR, falling back to the
        // plugin repo owner. The owner is a best-effort fallback, not
        // guaranteed to be a personal login — an org-owned repo's owner is
        // the org slug, which is not a user account. The webhook only echoes
        // the actor into the PR's provenance text, so an org slug here is
        // cosmetic rather than a hard failure.
        let env = ctx.env_source();
        let actor = env
            .var("GITHUB_ACTOR")
            .or_else(|| env.var("ANODIZER_GITHUB_ACTOR"))
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| plugin_owner.clone());
        let webhook_url = resolve_webhook_url(env);
        let request = KrewReleaseRequest::new(
            &format!("v{}", version),
            plugin_name,
            &plugin_owner,
            &plugin_repo,
            &actor,
            &manifest,
        );
        submit_krew_release_webhook(&webhook_url, &request, plugin_name, &version, log)?;
        return Ok(KrewPublishOutcome { pushed: false });
    }
    log.status(&format!(
        "publishing krew plugin '{}' via pr-direct",
        plugin_name
    ));

    let tmp_dir = tempfile::tempdir().context("krew: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        ctx,
        krew_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "krew",
        log,
    )?;

    // Write plugin manifest under plugins/<name>.yaml.
    let plugins_dir = repo_path.join("plugins");
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("krew: create plugins dir {}", plugins_dir.display()))?;

    let manifest_file = plugins_dir.join(format!("{}.yaml", plugin_name));
    std::fs::write(&manifest_file, &manifest)
        .with_context(|| format!("krew: write manifest {}", manifest_file.display()))?;

    log.status(&format!(
        "wrote Krew plugin manifest {}",
        manifest_file.display()
    ));

    let commit_msg = crate::homebrew::render_commit_msg(
        krew_cfg.commit_msg_template.as_deref(),
        plugin_name,
        &version,
        "plugin",
        log,
        ctx.render_is_strict(),
    )?;
    let branch_name = format!("{}-v{}", plugin_name, version);
    let commit_opts = util::resolve_commit_opts(ctx, krew_cfg.commit_author.as_ref(), log)?;
    // Always create a versioned branch for Krew PRs.
    let branch = Some(branch_name.as_str());
    let push_outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        branch,
        "krew",
        &commit_opts,
        log,
    )?;
    let pushed = match push_outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Krew manifest pushed to {}/{} branch '{}'",
                repo_owner, repo_name, branch_name
            ));
            true
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, krew manifest for '{}' already up to date",
                plugin_name
            ));
            false
        }
    };

    // Submit a PR. When `repository.pull_request` is configured, use the
    // unified PR helper (which respects `base`, `draft`, `body`); otherwise
    // submit a PR via `gh` CLI against the canonical kubernetes-sigs/krew-index
    // (or `repository.pull_request.base` when set).
    let has_pr_config = krew_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    let update_existing_pr = match krew_cfg.update_existing_pr.as_ref() {
        Some(v) => v
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("krew: render update_existing_pr condition")?,
        None => false,
    };

    // Clone the repository config so the PR submission helpers no
    // longer borrow from `ctx.config` (via `krew_cfg`). NLL then
    // drops the immutable borrow, making the subsequent `&mut ctx`
    // call legal.
    let repo_for_pr = krew_cfg.repository.clone();

    let pr_outcome = if has_pr_config {
        util::maybe_submit_pr_with_env(
            repo_path,
            repo_for_pr.as_ref(),
            &util::PrOrigin {
                repo_owner: &repo_owner,
                repo_name: &repo_name,
                branch_name: &branch_name,
                update_existing_pr,
            },
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\nAutomatically submitted by anodizer.",
                crate_name, version
            ),
            "krew",
            log,
            &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
            ctx.env_source(),
        )
    } else {
        // No `repository.pull_request:` block — always submit a PR against the
        // canonical kubernetes-sigs/krew-index slug (or the override in
        // `repository.pull_request.base`). Submitting against the user's own
        // fork would silently create useless intra-fork PRs against the user's
        // empty `main` branch instead of against the real upstream.
        let upstream_slug = repo_for_pr
            .as_ref()
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| match (base.owner.as_deref(), base.name.as_deref()) {
                (Some(o), Some(n)) => Some(format!("{}/{}", o, n)),
                _ => None,
            })
            .unwrap_or_else(|| "kubernetes-sigs/krew-index".to_string());

        util::submit_pr_via_gh_with_opts_with_env(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\nAutomatically submitted by anodizer.",
                crate_name, version
            ),
            "krew",
            log,
            util::SubmitPrOpts { update_existing_pr },
            ctx.env_source(),
        )
    };

    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(KrewPublishOutcome { pushed })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // krew-release-bot mode selection + webhook tests
    // -----------------------------------------------------------------------

    /// Explicit `mode: bot` / `mode: pr-direct` force the flow and skip
    /// the membership probe entirely (the probe would hit the network).
    #[test]
    fn explicit_mode_forces_flow_without_probe() {
        use anodizer_core::config::KrewMode;
        assert_eq!(
            detect_krew_flow(KrewMode::Bot, "anything", None).unwrap(),
            KrewFlow::BotWebhook
        );
        assert_eq!(
            detect_krew_flow(KrewMode::PrDirect, "anything", None).unwrap(),
            KrewFlow::PrDirect
        );
    }

    /// `auto` dispatch: definitive in-index → webhook; definitive absent
    /// → fork PR; INDETERMINATE probe → loud error (never a silent
    /// fork-PR fallback that krew maintainers reject).
    #[test]
    fn auto_probe_dispatch_errors_loudly_on_indeterminate() {
        assert_eq!(
            map_auto_probe("mytool", Some(true)).unwrap(),
            KrewFlow::BotWebhook
        );
        assert_eq!(
            map_auto_probe("mytool", Some(false)).unwrap(),
            KrewFlow::PrDirect
        );
        let err = map_auto_probe("mytool", None).unwrap_err().to_string();
        assert!(
            err.contains("could not determine krew-index membership"),
            "indeterminate probe must error: {err}"
        );
        // The hint must point at the explicit override + token remedies.
        assert!(
            err.contains("mode"),
            "error must mention the mode override: {err}"
        );
        assert!(
            err.contains("pr-direct") && err.contains("bot"),
            "error must name both explicit modes: {err}"
        );
    }

    /// The `ReleaseRequest` body carries the exact field names + values
    /// the bot's server-side struct expects, and base64-encodes the
    /// rendered manifest into `processedTemplate` (the server's `[]byte`
    /// JSON field).
    #[test]
    fn release_request_body_construction() {
        use base64::Engine as _;
        let manifest = "apiVersion: krew.googlecontainertools.github.com/v1alpha2\nkind: Plugin\n";
        let req = KrewReleaseRequest::new(
            "v1.2.3",
            "mytool",
            "acme",
            "mytool-repo",
            "octocat",
            manifest,
        );
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["tagName"], "v1.2.3");
        assert_eq!(v["pluginName"], "mytool");
        assert_eq!(v["pluginOwner"], "acme");
        assert_eq!(v["pluginRepo"], "mytool-repo");
        assert_eq!(v["pluginReleaseActor"], "octocat");
        assert_eq!(v["templateFile"], ".krew.yaml");
        // processedTemplate must be base64 of the raw manifest bytes so
        // the bot's Go `[]byte` decoder reconstructs the exact manifest.
        let expected = base64::engine::general_purpose::STANDARD.encode(manifest.as_bytes());
        assert_eq!(v["processedTemplate"], expected);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["processedTemplate"].as_str().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), manifest);
    }

    /// Webhook URL: env override wins; empty/unset falls back to the
    /// hosted default.
    #[test]
    fn webhook_url_resolution_honors_env_override() {
        use anodizer_core::env_source::MapEnvSource;
        let default_env = MapEnvSource::new();
        assert_eq!(
            resolve_webhook_url(&default_env),
            DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL
        );

        let custom_env = MapEnvSource::new().with(
            "KREW_RELEASE_BOT_WEBHOOK_URL",
            "https://krew.internal.example/webhook",
        );
        assert_eq!(
            resolve_webhook_url(&custom_env),
            "https://krew.internal.example/webhook"
        );

        let blank_env = MapEnvSource::new().with("KREW_RELEASE_BOT_WEBHOOK_URL", "  ");
        assert_eq!(
            resolve_webhook_url(&blank_env),
            DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL
        );
    }

    /// The already-submitted classifier matches ONLY the bot's actual
    /// duplicate-PR / clean-tree signals, and rejects every genuine
    /// failure — including bodies that merely contain loose phrases like
    /// `already exists`, so a future real error can't be swallowed.
    #[test]
    fn webhook_already_submitted_classifier() {
        // The benign signals the server emits.
        assert!(webhook_body_is_already_submitted(
            "opening pr: A pull request already exists for acme:mytool-v1.2.3"
        ));
        assert!(webhook_body_is_already_submitted(
            "opening pr: clean working tree, nothing to commit"
        ));
        assert!(webhook_body_is_already_submitted(
            "opening pr: clean working tree"
        ));

        // Genuine failures must NOT be swallowed.
        assert!(!webhook_body_is_already_submitted(
            "opening pr: failed when validating plugin spec"
        ));
        assert!(!webhook_body_is_already_submitted("internal server error"));
        // The loose arms dropped from the classifier: a bare resource
        // "already exists" or generic "up-to-date" is NOT the server's
        // duplicate-PR signal and must surface as a hard failure.
        assert!(!webhook_body_is_already_submitted(
            "opening pr: release already exists for tag v1.2.3"
        ));
        assert!(!webhook_body_is_already_submitted(
            "opening pr: branch already up-to-date with base"
        ));
    }

    /// HTTP 200 → success.
    #[test]
    fn webhook_submit_succeeds_on_200() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body =
            "PR \"https://github.com/kubernetes-sigs/krew-index/pull/42\" submitted successfully";
        let (addr, calls) = spawn_oneshot_http_responder(vec![Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )]);
        let url = format!("http://{addr}/github-action-webhook");
        let req =
            KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let r = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log);
        assert!(r.is_ok(), "200 must succeed: {r:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A non-200 whose body signals an already-existing PR is an
    /// idempotent no-op success.
    #[test]
    fn webhook_submit_idempotent_on_already_exists() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "opening pr: A pull request already exists for acme:mytool-v1.0.0";
        let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )]);
        let url = format!("http://{addr}/github-action-webhook");
        let req =
            KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let r = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log);
        assert!(
            r.is_ok(),
            "already-exists 500 must be a no-op success: {r:?}"
        );
    }

    /// A genuine failure (non-200, body not an already-exists signal)
    /// surfaces a loud error — krew must never silently skip.
    #[test]
    fn webhook_submit_errors_on_genuine_failure() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "opening pr: failed when validating plugin spec";
        let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )]);
        let url = format!("http://{addr}/github-action-webhook");
        let req =
            KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let err = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log)
            .expect_err("genuine 500 must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("500") && chain.contains("validating plugin spec"),
            "error must surface status + body: {chain}"
        );
    }

    // -----------------------------------------------------------------------
    // generate_manifest tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_manifest_basic() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "kubectl-mytool",
            version: "1.0.0",
            homepage: "https://github.com/org/mytool",
            short_description: "A kubectl plugin",
            description: "A great kubectl plugin for managing things.",
            caveats: "",
            platforms: &[
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
                    sha256: "deadbeef".to_string(),
                    bin: "kubectl-mytool".to_string(),
                    files: vec![],
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/mytool-darwin-amd64.tar.gz".to_string(),
                    sha256: "cafebabe".to_string(),
                    bin: "kubectl-mytool".to_string(),
                    files: vec![],
                },
            ],
        })
        .unwrap();

        // Header comment is present.
        assert!(manifest.starts_with("# This file was generated by anodizer. DO NOT EDIT.\n"));
        assert!(manifest.contains("apiVersion: krew.googlecontainertools.github.com/v1alpha2"));
        assert!(manifest.contains("kind: Plugin"));
        assert!(manifest.contains("  name: kubectl-mytool"));
        assert!(manifest.contains("version: v1.0.0"));
        assert!(manifest.contains("homepage: https://github.com/org/mytool"));
        assert!(manifest.contains("shortDescription: A kubectl plugin"));
        assert!(manifest.contains("A great kubectl plugin for managing things."));
        assert!(!manifest.contains("caveats:"));
        assert!(manifest.contains("platforms:"));
        assert!(manifest.contains("os: linux"));
        assert!(manifest.contains("arch: amd64"));
        assert!(manifest.contains("uri: https://example.com/mytool-linux-amd64.tar.gz"));
        assert!(manifest.contains("sha256: deadbeef"));
        assert!(manifest.contains("bin: kubectl-mytool"));
        assert!(manifest.contains("os: darwin"));
        assert!(manifest.contains("uri: https://example.com/mytool-darwin-amd64.tar.gz"));
        assert!(manifest.contains("sha256: cafebabe"));
    }

    /// The manifest `metadata.name` must carry the resolved `krew.name`
    /// override, not the crate name — krew-index CI rejects a plugin whose
    /// `metadata.name` disagrees with the declared plugin name / filename.
    #[test]
    fn manifest_name_uses_krew_name_override_not_crate_name() {
        // `resolve_plugin_name` picks the override over the crate name, and
        // renders it (here a no-op template render that returns its input).
        let plugin_name =
            resolve_plugin_name(Some("kubectl-mytool"), "mytool", |t| Ok(t.to_string())).unwrap();
        assert_eq!(plugin_name, "kubectl-mytool");

        let manifest = generate_manifest(&KrewManifestParams {
            name: &plugin_name,
            version: "1.0.0",
            homepage: "https://example.com",
            short_description: "A kubectl plugin",
            description: "desc",
            caveats: "",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/mytool.tar.gz".to_string(),
                sha256: "deadbeef".to_string(),
                bin: "kubectl-mytool".to_string(),
                files: vec![],
            }],
        })
        .unwrap();

        // Assert the exact `metadata.name` line (two-space indent under
        // `metadata:`), not a substring that formatting could satisfy
        // elsewhere.
        assert!(
            manifest.contains("\nmetadata:\n  name: kubectl-mytool\n"),
            "metadata.name must be the krew.name override; got:\n{manifest}"
        );
    }

    /// With no `krew.name` override, the plugin name falls back to the crate
    /// name (still rendered through the template engine).
    #[test]
    fn resolve_plugin_name_falls_back_to_crate_name() {
        let name = resolve_plugin_name(None, "mytool", |t| Ok(t.to_string())).unwrap();
        assert_eq!(name, "mytool");
    }

    /// A render failure in the plugin-name template propagates (it is not
    /// swallowed into a literal-template plugin name).
    #[test]
    fn resolve_plugin_name_propagates_render_error() {
        let err = resolve_plugin_name(Some("{{ bad"), "mytool", |_| {
            anyhow::bail!("template parse error")
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("render plugin name template"),
            "render error must be contextualized; got: {err}"
        );
    }

    #[test]
    fn test_generate_manifest_with_caveats() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "my-plugin",
            version: "2.0.0",
            homepage: "https://example.com",
            short_description: "Plugin",
            description: "A plugin",
            caveats: "Run 'kubectl my-plugin init' after installation.",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/plugin.tar.gz".to_string(),
                sha256: "hash".to_string(),
                bin: "kubectl-my-plugin".to_string(),
                files: vec![],
            }],
        })
        .unwrap();

        assert!(manifest.contains("caveats:"));
        assert!(manifest.contains("Run 'kubectl my-plugin init' after installation."));
    }

    #[test]
    fn test_generate_manifest_no_homepage() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "tool",
            version: "1.0.0",
            homepage: "",
            short_description: "A tool",
            description: "desc",
            caveats: "",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/tool.tar.gz".to_string(),
                sha256: "hash".to_string(),
                bin: "kubectl-tool".to_string(),
                files: vec![],
            }],
        })
        .unwrap();

        assert!(!manifest.contains("homepage:"));
    }

    #[test]
    fn test_generate_manifest_multi_platform() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "multi",
            version: "1.0.0",
            homepage: "https://example.com",
            short_description: "Multi-platform plugin",
            description: "A plugin for all platforms.",
            caveats: "",
            platforms: &[
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-linux-amd64.tar.gz".to_string(),
                    sha256: "hash_linux_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                    files: vec![],
                },
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "arm64".to_string(),
                    url: "https://example.com/multi-linux-arm64.tar.gz".to_string(),
                    sha256: "hash_linux_arm64".to_string(),
                    bin: "kubectl-multi".to_string(),
                    files: vec![],
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-darwin-amd64.tar.gz".to_string(),
                    sha256: "hash_darwin_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                    files: vec![],
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "arm64".to_string(),
                    url: "https://example.com/multi-darwin-arm64.tar.gz".to_string(),
                    sha256: "hash_darwin_arm64".to_string(),
                    bin: "kubectl-multi".to_string(),
                    files: vec![],
                },
                KrewPlatform {
                    os: "windows".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-windows-amd64.zip".to_string(),
                    sha256: "hash_windows_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                    files: vec![],
                },
            ],
        })
        .unwrap();

        // Count platform entries (each starts with "- selector:")
        let platform_count = manifest.matches("- selector:").count();
        assert_eq!(platform_count, 5);

        // Verify all platforms present
        assert!(manifest.contains("hash_linux_amd64"));
        assert!(manifest.contains("hash_linux_arm64"));
        assert!(manifest.contains("hash_darwin_amd64"));
        assert!(manifest.contains("hash_darwin_arm64"));
        assert!(manifest.contains("hash_windows_amd64"));
    }

    #[test]
    fn test_generate_manifest_complete_structure() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "kubectl-anodizer",
            version: "3.2.1",
            homepage: "https://github.com/tj-smith47/anodizer",
            short_description: "Release automation as a kubectl plugin",
            description: "A comprehensive release automation tool\nfor Kubernetes-based projects.",
            caveats: "Ensure kubectl is configured before use.",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-amd64.tar.gz".to_string(),
                sha256: "aabbccdd".to_string(),
                bin: "kubectl-anodizer".to_string(),
                files: vec![],
            }],
        }).unwrap();

        // Starts with header comment
        assert!(manifest.starts_with("# This file was generated by anodizer. DO NOT EDIT.\n"));

        // Verify structure order (line 0 is the header comment)
        let lines: Vec<&str> = manifest.lines().collect();
        assert_eq!(
            lines[0],
            "# This file was generated by anodizer. DO NOT EDIT."
        );
        assert_eq!(
            lines[1],
            "apiVersion: krew.googlecontainertools.github.com/v1alpha2"
        );
        assert_eq!(lines[2], "kind: Plugin");
        assert_eq!(lines[3], "metadata:");
        assert_eq!(lines[4], "  name: kubectl-anodizer");
        assert_eq!(lines[5], "spec:");
        assert!(lines[6].contains("version: v3.2.1"));

        // Multi-line description
        assert!(manifest.contains("A comprehensive release automation tool"));
        assert!(manifest.contains("for Kubernetes-based projects."));

        // Caveats
        assert!(manifest.contains("Ensure kubectl is configured before use."));
    }

    // -----------------------------------------------------------------------
    // krew_arch / krew_os helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_krew_arch_mapping() {
        assert_eq!(krew_arch("amd64"), "amd64");
        assert_eq!(krew_arch("x86_64"), "amd64");
        assert_eq!(krew_arch("arm64"), "arm64");
        assert_eq!(krew_arch("aarch64"), "arm64");
        assert_eq!(krew_arch("unknown"), "unknown");
    }

    #[test]
    fn test_krew_os_mapping() {
        assert_eq!(krew_os("darwin"), "darwin");
        assert_eq!(krew_os("macos"), "darwin");
        assert_eq!(krew_os("linux"), "linux");
        assert_eq!(krew_os("windows"), "windows");
        assert_eq!(krew_os("freebsd"), "freebsd");
    }

    // -----------------------------------------------------------------------
    // publish_to_krew dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_krew_missing_config() {
        use anodizer_core::config::{Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        assert!(publish_to_krew(&mut ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_krew_missing_manifests_repo() {
        use anodizer_core::config::{Config, CrateConfig, KrewConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: None, // Missing
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        assert!(publish_to_krew(&mut ctx, "mytool", &log).is_err());
    }

    // -----------------------------------------------------------------------
    // artifacts_to_platforms .exe / binary-name-resolution tests
    // -----------------------------------------------------------------------

    fn make_os_artifact(os: &str, arch: &str, binary: Option<&str>) -> OsArtifact {
        OsArtifact {
            url: format!("https://example.com/{}-{}.tar.gz", os, arch),
            sha256: "deadbeef".into(),
            os: os.into(),
            arch: arch.into(),
            binary: binary.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_artifacts_to_platforms_appends_exe_for_windows() {
        let arts = vec![
            make_os_artifact("linux", "amd64", Some("cfgd")),
            make_os_artifact("windows", "amd64", Some("cfgd")),
            make_os_artifact("darwin", "arm64", Some("cfgd")),
        ];
        let plats = artifacts_to_platforms(&arts, "cfgd");
        let by_os = |os: &str| plats.iter().find(|p| p.os == os).expect("missing platform");
        assert_eq!(by_os("linux").bin, "cfgd");
        assert_eq!(by_os("windows").bin, "cfgd.exe");
        assert_eq!(by_os("darwin").bin, "cfgd");
    }

    #[test]
    fn test_artifacts_to_platforms_does_not_double_suffix_exe() {
        let arts = vec![make_os_artifact("windows", "amd64", Some("cfgd.exe"))];
        let plats = artifacts_to_platforms(&arts, "cfgd");
        assert_eq!(plats[0].bin, "cfgd.exe");
    }

    #[test]
    fn test_artifacts_to_platforms_uses_archive_binary_name_over_default() {
        // Crate is `kubectl-cfgd` but ships binary `cfgd` — the manifest
        // must point at the in-archive name, not the crate name.
        let arts = vec![make_os_artifact("linux", "amd64", Some("cfgd"))];
        let plats = artifacts_to_platforms(&arts, "kubectl-cfgd");
        assert_eq!(plats[0].bin, "cfgd");
    }

    #[test]
    fn test_artifacts_to_platforms_falls_back_to_default_when_binary_unset() {
        let arts = vec![make_os_artifact("linux", "amd64", None)];
        let plats = artifacts_to_platforms(&arts, "cfgd");
        assert_eq!(plats[0].bin, "cfgd");
    }

    /// Build an `OsArtifact` with explicit wrap-prefix + bundled file list so
    /// the `files:` derivation can be exercised across flat / nested layouts.
    fn make_os_artifact_full(
        os: &str,
        arch: &str,
        binary: Option<&str>,
        wrap: Option<&str>,
        archive_files: &[&str],
    ) -> OsArtifact {
        OsArtifact {
            binary: binary.map(str::to_string),
            wrap_in_directory: wrap.map(str::to_string),
            archive_files: archive_files.iter().map(|s| s.to_string()).collect(),
            ..make_os_artifact(os, arch, binary)
        }
    }

    // -----------------------------------------------------------------------
    // `files:` extraction-list derivation (binary + LICENSE + README)
    // -----------------------------------------------------------------------

    /// Flat archive (no wrap dir): the binary `from` is just the binary name,
    /// LICENSE + README are picked up from the bundled file set, all `to: "."`.
    #[test]
    fn derive_krew_files_flat_archive_binary_license_readme() {
        let a = make_os_artifact_full(
            "linux",
            "amd64",
            Some("cfgd"),
            None,
            &["LICENSE", "README.md"],
        );
        let files = derive_krew_files(&a, "cfgd");
        assert_eq!(
            files,
            vec![
                KrewFileEntry {
                    from: "cfgd".into(),
                    to: ".".into()
                },
                KrewFileEntry {
                    from: "LICENSE".into(),
                    to: ".".into()
                },
                KrewFileEntry {
                    from: "README.md".into(),
                    to: ".".into()
                },
            ]
        );
    }

    /// Nested archive (`wrap_in_directory`): both the binary AND the bundled
    /// LICENSE/README `from` paths must carry the wrap prefix, or krew's
    /// extractor cannot find them ("source binary cannot be found").
    #[test]
    fn derive_krew_files_nested_archive_prefixes_from_paths() {
        let a = make_os_artifact_full(
            "linux",
            "amd64",
            Some("cfgd"),
            Some("cfgd-1.0.0-linux-amd64"),
            &["cfgd-1.0.0-linux-amd64/LICENSE"],
        );
        let files = derive_krew_files(&a, "cfgd");
        assert_eq!(
            files,
            vec![
                KrewFileEntry {
                    from: "cfgd-1.0.0-linux-amd64/cfgd".into(),
                    to: ".".into()
                },
                KrewFileEntry {
                    from: "cfgd-1.0.0-linux-amd64/LICENSE".into(),
                    to: ".".into()
                },
            ]
        );
    }

    /// Windows `.exe` handling carries into the `files[].from` binary entry.
    #[test]
    fn derive_krew_files_windows_exe_in_from() {
        let a = make_os_artifact_full("windows", "amd64", Some("cfgd"), None, &["LICENSE"]);
        // The resolved bin (with `.exe`) is what artifacts_to_platforms passes in.
        let plats = artifacts_to_platforms(&[a], "cfgd");
        let win = &plats[0];
        assert_eq!(win.bin, "cfgd.exe");
        assert_eq!(win.files[0].from, "cfgd.exe");
        assert_eq!(win.files[0].to, ".");
        assert_eq!(win.files[1].from, "LICENSE");
    }

    /// LICENSE/README entries are GATED on actual archive presence: an archive
    /// bundling no extra files yields a `files:` list with only the binary.
    #[test]
    fn derive_krew_files_absent_license_not_emitted() {
        let a = make_os_artifact_full("linux", "amd64", Some("cfgd"), None, &[]);
        let files = derive_krew_files(&a, "cfgd");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].from, "cfgd");
    }

    /// CHANGELOG (even `CHANGELOG.md`, the common case stage-archive bundles by
    /// default) + non-license non-markdown bundled files are NOT pulled into the
    /// krew `files:` list — only the binary, LICENSE, and non-changelog `*.md`.
    #[test]
    fn derive_krew_files_ignores_changelog_and_completions() {
        let a = make_os_artifact_full(
            "linux",
            "amd64",
            Some("cfgd"),
            None,
            &[
                "LICENSE",
                "CHANGELOG.md",
                "completions/cfgd.bash",
                "README.md",
            ],
        );
        let froms: Vec<String> = derive_krew_files(&a, "cfgd")
            .iter()
            .map(|f| f.from.clone())
            .collect();
        assert_eq!(froms, vec!["cfgd", "LICENSE", "README.md"]);
        assert!(
            !froms.iter().any(|f| f.contains("CHANGELOG")),
            "CHANGELOG.md must not leak into the krew files: list, got {froms:?}"
        );
    }

    /// `LICENSE.md` matches BOTH the license glob and the `*.md` README filter;
    /// it must be emitted exactly ONCE (krew would otherwise copy it twice).
    #[test]
    fn derive_krew_files_license_md_emitted_once() {
        let a = make_os_artifact_full(
            "linux",
            "amd64",
            Some("cfgd"),
            None,
            &["LICENSE.md", "README.md"],
        );
        let froms: Vec<String> = derive_krew_files(&a, "cfgd")
            .iter()
            .map(|f| f.from.clone())
            .collect();
        assert_eq!(
            froms,
            vec!["cfgd", "LICENSE.md", "README.md"],
            "LICENSE.md must appear once (as the license), README.md once"
        );
        assert_eq!(
            froms.iter().filter(|f| f.as_str() == "LICENSE.md").count(),
            1,
            "LICENSE.md must not be duplicated, got {froms:?}"
        );
    }

    /// LICENSE matching is case-insensitive (`license`, `LICENSE.txt`, …).
    #[test]
    fn derive_krew_files_license_case_insensitive() {
        let a = make_os_artifact_full("linux", "amd64", Some("cfgd"), None, &["license.txt"]);
        let files = derive_krew_files(&a, "cfgd");
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].from, "license.txt");
    }

    // -----------------------------------------------------------------------
    // shortDescription length validation
    // -----------------------------------------------------------------------

    /// A tagline within the krew-index norm produces no warning.
    #[test]
    fn short_description_within_norm_no_warning() {
        let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
        warn_if_short_description_too_long("Switch between contexts", "ctx", &log);
        assert_eq!(cap.warn_count(), 0);
    }

    /// A tagline exceeding ~50 chars warns loudly, naming the field, the crate,
    /// and the actual length so the user can shorten it before krew-index review.
    #[test]
    fn short_description_too_long_warns_with_field_and_length() {
        let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
        let long = "This is an excessively long krew plugin tagline that will surely be flagged";
        assert!(long.chars().count() > KREW_SHORT_DESCRIPTION_MAX);
        warn_if_short_description_too_long(long, "mytool", &log);
        assert_eq!(cap.warn_count(), 1);
        let msg = cap.warn_messages().join("\n");
        assert!(msg.contains("shortDescription"), "names the field: {msg}");
        assert!(msg.contains("mytool"), "names the crate: {msg}");
        assert!(
            msg.contains(&long.chars().count().to_string()),
            "states the actual length: {msg}"
        );
    }

    /// Boundary: exactly the max length does NOT warn; one over does.
    #[test]
    fn short_description_boundary_is_inclusive() {
        let at = "x".repeat(KREW_SHORT_DESCRIPTION_MAX);
        let over = "x".repeat(KREW_SHORT_DESCRIPTION_MAX + 1);
        let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
        warn_if_short_description_too_long(&at, "c", &log);
        assert_eq!(cap.warn_count(), 0, "exactly max must not warn");
        warn_if_short_description_too_long(&over, "c", &log);
        assert_eq!(cap.warn_count(), 1, "one over max must warn");
    }

    #[test]
    fn test_artifacts_to_platforms_arch_all_expands_with_correct_bin() {
        // arch=all should expand to amd64+arm64 with the same bin name on
        // both. Not a Windows test (krew doesn't use arch=all on windows
        // in practice) — just confirms the expansion path also flows
        // through resolve_bin.
        let arts = vec![make_os_artifact("darwin", "all", Some("cfgd"))];
        let plats = artifacts_to_platforms(&arts, "cfgd");
        assert_eq!(plats.len(), 2);
        assert!(plats.iter().all(|p| p.bin == "cfgd"));
        let arches: Vec<_> = plats.iter().map(|p| p.arch.as_str()).collect();
        assert!(arches.contains(&"amd64"));
        assert!(arches.contains(&"arm64"));
    }

    /// `krew.skip_upload: "{{ .IsSnapshot }}"` must template-expand
    /// before its bool/auto/empty interpretation. On a snapshot run
    /// the rendered value is `"true"` and the publish path must
    /// short-circuit to `Ok(())` BEFORE the missing-repository check.
    #[test]
    fn krew_skip_upload_template_expands_to_true_on_snapshot() {
        use anodizer_core::config::{Config, CrateConfig, KrewConfig, PublishConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.project_name = "mytool".to_string();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    // repository intentionally None — would normally
                    // hard-fail with "no repository config", but the
                    // skip_upload short-circuit must run BEFORE the
                    // repository-missing check.
                    repository: None,
                    skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("IsSnapshot", "true");

        let log = StageLogger::new("publish", Verbosity::Normal);
        publish_to_krew(&mut ctx, "mytool", &log).expect(
            "skip_upload='{{ .IsSnapshot }}' on snapshot must short-circuit \
             to Ok(()) before the repository-missing check (GR cba5b9f)",
        );
    }

    // =====================================================================
    // PUBLISH FLOW — render_krew_manifest_for_crate + publish_to_krew's
    // PrDirect clone→write→commit→push→PR path and its error/classifier
    // boundaries.
    //
    // The PrDirect end-to-end tests drive the live publish against a local
    // bare git repo: `repository.git.url` points the clone at a `file`
    // path (no network), and the PR-submission transport is forced onto an
    // in-process scripted responder by installing a failing `gh` stub
    // (so `gh_is_available()` is false) and injecting
    // `ANODIZER_GITHUB_API_BASE` at the responder. These tests still mutate
    // PATH (the `gh` stub), so each is `#[serial(path_env)]`.
    //
    // The krew publish path threads PR submission through the Context's
    // injectable `EnvSource` (`maybe_submit_pr_with_env` /
    // `submit_pr_via_gh_with_opts_with_env`), so the responder address is a
    // per-Context value set via `inject_api_base` — not a process-global
    // mutation. Tokens come from `repository.token` config.
    // =====================================================================

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, CrateConfig, GitRepoConfig, KrewConfig, KrewMode, PublishConfig, PullRequestConfig,
        ReleaseConfig, RepositoryConfig, ScmRepoConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::sync::OnceLock;

    fn quiet() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    /// Give the test process a git identity + non-interactive credential
    /// behaviour so the publish path's `git commit` / cross-repo
    /// `git fetch` work on a bare CI runner. One-shot per process.
    fn ensure_git_identity() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            // SAFETY: runs once per process under OnceLock; constants only.
            unsafe {
                std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_TERMINAL_PROMPT", "0"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            }
        });
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Build a bare "krew-index fork" repo with one commit on `main`, the
    /// branch the publish path's clone (`--depth=1`) defaults to. Returns
    /// `(bare_path_string, _bare_holder)`. The PrDirect publish clones
    /// this, writes `plugins/<name>.yaml`, commits a versioned branch, and
    /// pushes it back here.
    fn init_bare_fork() -> (String, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        git_ok(bare.path(), &["init", "--bare", "-b", "main"]);
        git_ok(seed.path(), &["init", "-b", "main"]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "Test"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "krew-index\n").unwrap();
        git_ok(seed.path(), &["add", "README"]);
        git_ok(seed.path(), &["commit", "-m", "seed"]);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        git_ok(seed.path(), &["push", "-u", "origin", "main"]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// A `gh` stub that exits non-zero on `--version` so
    /// `gh_is_available()` is false → the PR transport falls to the API
    /// path. Returns the guard (restores PATH + holds the env mutex for
    /// the test's lifetime) + the on-disk stub holder.
    fn gh_absent() -> (
        FakeToolDir,
        anodizer_core::test_helpers::fake_tool::PathGuard,
    ) {
        let tools = FakeToolDir::new();
        tools.tool("gh").exit(1).install();
        let guard = tools.activate();
        (tools, guard)
    }

    /// Point the scripted responder's address at the krew PR path by
    /// injecting `ANODIZER_GITHUB_API_BASE` into the Context's env source.
    /// The base is per-Context, not process-global, so no env mutation and
    /// no teardown is needed; PATH stays process-global via the
    /// `gh_absent`/`gh_present` `PathGuard`.
    fn inject_api_base(ctx: &mut Context, addr: &std::net::SocketAddr) {
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new()
                .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}")),
        );
    }

    /// Register one archive artifact carrying the `url` / `sha256` /
    /// `extra_binaries` metadata the manifest's `platforms[]` block reads.
    /// Mirrors the schema-validation test helper so the manifest the live
    /// publish renders is the same byte-for-byte shape.
    fn add_archive(
        ctx: &mut Context,
        crate_name: &str,
        target: &str,
        os: &str,
        arch: &str,
        binary: &str,
        sha: &str,
    ) {
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!("https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{os}-{arch}.tar.gz"),
        );
        meta.insert("sha256".to_string(), sha.to_string());
        meta.insert("format".to_string(), "tar.gz".to_string());
        meta.insert("extra_binaries".to_string(), binary.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{binary}-{os}-{arch}.tar.gz")),
            name: format!("{binary}-{os}-{arch}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// A crate whose krew block clones from a local bare repo (`git.url`)
    /// and PRs same-repo (so no cross-repo fork-sync), forcing the API
    /// transport when `gh` is absent. `mode: pr-direct` skips the
    /// network membership probe.
    fn pr_direct_crate(crate_name: &str, plugin: &str, bare_url: &str) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    name: Some(plugin.to_string()),
                    mode: Some(KrewMode::PrDirect),
                    repository: Some(RepositoryConfig {
                        owner: Some("fork-owner".to_string()),
                        name: Some("krew-index".to_string()),
                        token: Some("ghp_test".to_string()),
                        git: Some(GitRepoConfig {
                            url: Some(bare_url.to_string()),
                            ssh_command: None,
                            private_key: None,
                        }),
                        pull_request: Some(PullRequestConfig {
                            enabled: Some(true),
                            // No `base` => upstream == fork => same-repo,
                            // no fork-sync side effect on the bare repo.
                            base: None,
                            draft: None,
                            body: None,
                        }),
                        ..Default::default()
                    }),
                    description: Some("A widget management kubectl plugin.".to_string()),
                    short_description: Some("Manage widgets from kubectl".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn build_ctx(crates: Vec<CrateConfig>, version: &str) -> Context {
        let config = Config {
            crates,
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
        ctx
    }

    // -----------------------------------------------------------------
    // render_krew_manifest_for_crate — skip / error boundaries that the
    // publish path short-circuits on before any clone.
    // -----------------------------------------------------------------

    /// `skip: true` short-circuits the renderer to `None` (the publisher
    /// renders nothing for this crate). Asserts the gate fires BEFORE the
    /// required-artifact / repository checks — there are no artifacts here,
    /// yet the call is `Ok(None)`, not an error.
    #[test]
    fn render_manifest_skip_true_returns_none() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let ctx = build_ctx(vec![c], "1.0.0");
        let out = render_krew_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
        assert!(out.is_none(), "skip=true must render nothing, got {out:?}");
    }

    /// A falsy `if:` condition short-circuits the renderer to `None`,
    /// same as `skip` — proving the `if` gate is evaluated and honored.
    #[test]
    fn render_manifest_falsy_if_returns_none() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.if_condition = Some("false".to_string());
        }
        let ctx = build_ctx(vec![c], "1.0.0");
        let out = render_krew_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
        assert!(out.is_none(), "falsy `if` must render nothing, got {out:?}");
    }

    /// A crate with no description anywhere (no krew.description, no
    /// Cargo.toml fallback) bails with the actionable "description is not
    /// set" message — the manifest's required narrative field.
    #[test]
    fn render_manifest_missing_description_bails() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.description = None;
            k.short_description = None;
        }
        let ctx = build_ctx(vec![c], "1.0.0");
        let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect_err("missing description must bail");
        assert!(
            format!("{err:#}").contains("description is not set"),
            "got: {err:#}"
        );
    }

    /// No archive artifacts → hard error (a manifest with no real
    /// platforms is unusable). The message must name the crate and point
    /// at adding targets / removing the publisher.
    #[test]
    fn render_manifest_no_artifacts_bails() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let ctx = build_ctx(vec![c], "1.0.0");
        let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect_err("no artifacts must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("no archive artifacts"), "got: {msg}");
        assert!(msg.contains("widget"), "must name the crate: {msg}");
    }

    /// More than one binary in a single archive → bail (krew allows
    /// exactly one binary per platform). `extra_binaries` is a
    /// COMMA-separated list (`Artifact::extra_binaries` splits on `,`), so
    /// two comma-joined names must trip the one-binary-per-archive guard
    /// with the count in the message.
    #[test]
    fn render_manifest_multi_binary_archive_bails() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            "https://example.com/widget-linux-amd64.tar.gz".to_string(),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert(
            "extra_binaries".to_string(),
            "kubectl-widget,kubectl-extra".to_string(),
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-linux-amd64.tar.gz"),
            name: "widget-linux-amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });
        let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect_err("multi-binary archive must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("only one binary per archive"), "got: {msg}");
        assert!(msg.contains("got 2"), "must report the count: {msg}");
    }

    /// The rendered manifest carries the crate's real sha256 + url from
    /// the registered artifact (not a placeholder), with the resolved
    /// plugin-name override in `metadata.name`. Pins the field plumbing
    /// from artifact metadata → manifest YAML end-to-end.
    #[test]
    fn render_manifest_embeds_real_sha256_and_url() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let sha = "b".repeat(64);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &sha,
        );
        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest.contains(&format!("sha256: {sha}")),
            "manifest must embed the artifact's real sha256; got:\n{manifest}"
        );
        assert!(
            manifest.contains(
                "uri: https://github.com/acme/widget/releases/download/v1.0.0/kubectl-widget-linux-amd64.tar.gz"
            ),
            "manifest must embed the artifact url; got:\n{manifest}"
        );
        assert!(
            manifest.contains("\nmetadata:\n  name: kubectl-widget\n"),
            "metadata.name carries the krew.name override; got:\n{manifest}"
        );
        assert!(manifest.contains("version: v1.0.0"), "got:\n{manifest}");
    }

    /// Register an archive carrying the full layout metadata the krew `files:`
    /// derivation reads: `wrap_in_directory` (nesting prefix) and `archive_files`
    /// (bundled non-binary in-archive paths). Mirrors what stage-archive writes.
    #[allow(clippy::too_many_arguments)]
    fn add_archive_full(
        ctx: &mut Context,
        crate_name: &str,
        target: &str,
        os: &str,
        arch: &str,
        binary: &str,
        sha: &str,
        wrap: Option<&str>,
        archive_files: &[&str],
    ) {
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!("https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{os}-{arch}.tar.gz"),
        );
        meta.insert("sha256".to_string(), sha.to_string());
        meta.insert("format".to_string(), "tar.gz".to_string());
        meta.insert("extra_binaries".to_string(), binary.to_string());
        if let Some(w) = wrap {
            meta.insert("wrap_in_directory".to_string(), w.to_string());
        }
        if !archive_files.is_empty() {
            meta.insert("archive_files".to_string(), archive_files.join(","));
        }
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{binary}-{os}-{arch}.tar.gz")),
            name: format!("{binary}-{os}-{arch}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// SINGLE-CRATE: a flat-layout linux archive + a windows archive must each
    /// emit a concrete per-platform `files:` block — the binary (`.exe` on
    /// windows) plus the bundled LICENSE — matching the krew-index exemplar shape
    /// (`from`/`to: "."`). Asserts the exact rendered YAML, not a round-trip.
    #[test]
    fn render_manifest_emits_files_block_single_crate_linux_and_windows() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let sha = "c".repeat(64);
        add_archive_full(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &sha,
            None,
            &["LICENSE"],
        );
        add_archive_full(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "windows",
            "amd64",
            "kubectl-widget",
            &sha,
            None,
            &["LICENSE"],
        );
        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");

        // Linux platform: binary `from` is the flat binary name (no `.exe`),
        // followed by the LICENSE entry, both `to: "."`.
        let linux_block = "\
    bin: kubectl-widget
    files:
    - from: kubectl-widget
      to: .
    - from: LICENSE
      to: .";
        assert!(
            manifest.contains(linux_block),
            "linux files block must select binary + LICENSE; got:\n{manifest}"
        );

        // Windows platform: binary `from` carries the `.exe` suffix.
        let windows_block = "\
    bin: kubectl-widget.exe
    files:
    - from: kubectl-widget.exe
      to: .
    - from: LICENSE
      to: .";
        assert!(
            manifest.contains(windows_block),
            "windows files block must use the `.exe` binary name; got:\n{manifest}"
        );
    }

    /// SINGLE-CRATE nested layout: when the archive wraps its contents in a
    /// top-level dir (`wrap_in_directory`), BOTH the binary and the LICENSE
    /// `from` paths must carry that prefix, or krew's extractor fails to find
    /// the binary on install.
    #[test]
    fn render_manifest_files_block_respects_nested_archive_layout() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let sha = "d".repeat(64);
        add_archive_full(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &sha,
            Some("kubectl-widget-1.0.0"),
            &[
                "kubectl-widget-1.0.0/LICENSE",
                "kubectl-widget-1.0.0/README.md",
            ],
        );
        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        let nested_block = "\
    files:
    - from: kubectl-widget-1.0.0/kubectl-widget
      to: .
    - from: kubectl-widget-1.0.0/LICENSE
      to: .
    - from: kubectl-widget-1.0.0/README.md
      to: .";
        assert!(
            manifest.contains(nested_block),
            "nested-layout files must carry the wrap prefix on every `from`; got:\n{manifest}"
        );
    }

    /// WORKSPACE PER-CRATE: two crates published in one run resolve their own
    /// binary name AND their own `files:` list independently — no cross-crate
    /// leakage. Each crate ships a distinct binary and a distinct bundled file.
    #[test]
    fn render_manifest_files_block_per_crate_no_cross_leakage() {
        let alpha = pr_direct_crate("alpha", "kubectl-alpha", "/unused");
        let beta = pr_direct_crate("beta", "kubectl-beta", "/unused");
        let mut ctx = build_ctx(vec![alpha, beta], "1.0.0");
        let sha = "e".repeat(64);
        // alpha ships binary `alpha-bin` and bundles only a LICENSE.
        add_archive_full(
            &mut ctx,
            "alpha",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "alpha-bin",
            &sha,
            None,
            &["LICENSE"],
        );
        // beta ships binary `beta-bin` and bundles a LICENSE + README.
        add_archive_full(
            &mut ctx,
            "beta",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "beta-bin",
            &sha,
            None,
            &["LICENSE", "README.md"],
        );

        let alpha_manifest = render_krew_manifest_for_crate(&ctx, "alpha", &quiet())
            .expect("alpha render ok")
            .expect("alpha not skipped");
        let beta_manifest = render_krew_manifest_for_crate(&ctx, "beta", &quiet())
            .expect("beta render ok")
            .expect("beta not skipped");

        // alpha: its OWN binary, its OWN (LICENSE-only) files list.
        assert!(
            alpha_manifest.contains(
                "\
    files:
    - from: alpha-bin
      to: .
    - from: LICENSE
      to: ."
            ),
            "alpha files must select alpha-bin + LICENSE; got:\n{alpha_manifest}"
        );
        assert!(
            !alpha_manifest.contains("beta-bin") && !alpha_manifest.contains("README.md"),
            "alpha manifest must not leak beta's binary or README; got:\n{alpha_manifest}"
        );

        // beta: its OWN binary, with the extra README entry alpha does not have.
        assert!(
            beta_manifest.contains(
                "\
    files:
    - from: beta-bin
      to: .
    - from: LICENSE
      to: .
    - from: README.md
      to: ."
            ),
            "beta files must select beta-bin + LICENSE + README; got:\n{beta_manifest}"
        );
        assert!(
            !beta_manifest.contains("alpha-bin"),
            "beta manifest must not leak alpha's binary; got:\n{beta_manifest}"
        );
    }

    // -----------------------------------------------------------------
    // crate_has_krew_artifacts — eligibility predicate.
    // -----------------------------------------------------------------

    /// `crate_has_krew_artifacts` is true once an eligible archive exists
    /// and false on an empty artifact set — the live path errors and the
    /// offline validator skips on the same `false` signal.
    #[test]
    fn crate_has_krew_artifacts_reflects_artifact_presence() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        let krew_cfg = c
            .publish
            .as_ref()
            .and_then(|p| p.krew.clone())
            .expect("krew cfg");
        let mut ctx = build_ctx(vec![c], "1.0.0");
        assert!(
            !crate_has_krew_artifacts(&ctx, "widget", &krew_cfg).unwrap(),
            "no archives => not eligible"
        );
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        assert!(
            crate_has_krew_artifacts(&ctx, "widget", &krew_cfg).unwrap(),
            "one archive => eligible"
        );
    }

    // -----------------------------------------------------------------
    // publish_to_krew — PrDirect end-to-end against a local bare repo.
    // -----------------------------------------------------------------

    /// Full PrDirect single-crate publish: clone the (local) fork, write
    /// `plugins/<plugin>.yaml`, commit a `<plugin>-v<version>` branch,
    /// push it to the bare repo, then submit the PR via the API
    /// transport. Asserts BOTH real side effects:
    ///   (1) the bare repo gained the versioned branch carrying the
    ///       manifest file with the crate's real sha256, and
    ///   (2) the PR-create POST reached the responder at the same-repo
    ///       `/repos/fork-owner/krew-index/pulls` with head = fork:branch.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_to_krew_pr_direct_pushes_branch_and_opens_pr() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);
        let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        let sha = "c".repeat(64);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &sha,
        );

        let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
        assert!(
            outcome.pushed,
            "PrDirect publish must report a real push (drives any_pushed gate)"
        );

        // (1) The versioned branch landed in the bare repo, carrying the
        //     manifest file with the real sha256.
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains("kubectl-widget-v1.0.0"),
            "publish must push the versioned branch; bare branches:\n{branches}"
        );
        let manifest_in_repo = git_stdout(
            bare.path(),
            &["show", "kubectl-widget-v1.0.0:plugins/kubectl-widget.yaml"],
        );
        assert!(
            manifest_in_repo.contains(&format!("sha256: {sha}")),
            "pushed manifest must carry the real sha256; got:\n{manifest_in_repo}"
        );
        assert!(
            manifest_in_repo.contains("name: kubectl-widget"),
            "pushed manifest metadata.name; got:\n{manifest_in_repo}"
        );

        // (2) The PR-create POST hit the same-repo upstream slug.
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
        assert_eq!(entries[0].path, "/repos/fork-owner/krew-index/pulls");
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(
            payload["head"], "fork-owner:kubectl-widget-v1.0.0",
            "head must be fork-owner:<plugin>-v<version>"
        );
        drop(entries);
        drop(bare);
    }

    /// PrDirect publish when the upstream PR already exists: the API
    /// transport returns 422 "already exists" and the publisher records a
    /// `PendingValidation` override (so the dispatch summary tells the
    /// truth instead of reporting `succeeded`). The branch push still
    /// happened, so `pushed` is true.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_to_krew_pr_direct_already_exists_records_pending() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:kubectl-widget-v1.0.0.\"}]}";
        let (addr, _req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: Box::leak(
                format!(
                    "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .into_boxed_str(),
            ),
            times: Some(1),
        }]);
        let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"d".repeat(64),
        );

        let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
        assert!(outcome.pushed, "branch push happened before the PR call");
        let pending = ctx.take_pending_outcome();
        assert!(
            matches!(
                pending,
                Some(anodizer_core::PublisherOutcome::PendingValidation)
            ),
            "422 already-exists must record PendingValidation, got {pending:?}"
        );
        drop(bare);
    }

    /// Idempotent re-publish: when the bare fork already carries the exact
    /// versioned branch + identical manifest, `commit_and_push_with_opts`
    /// detects the unchanged tree and reports `NoChanges`, so the publish
    /// outcome's `pushed` is false (nothing to roll back). The PR is not
    /// re-submitted side-effect-wise; we assert the no-push outcome.
    #[test]
    #[serial(path_env)]
    fn publish_to_krew_pr_direct_idempotent_no_changes() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        // First publish pushes the branch.
        let (addr, _l1) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let sha = "e".repeat(64);
        let build = || {
            let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
            let mut ctx = build_ctx(vec![c], "1.0.0");
            inject_api_base(&mut ctx, &addr);
            add_archive(
                &mut ctx,
                "widget",
                "x86_64-unknown-linux-gnu",
                "linux",
                "amd64",
                "kubectl-widget",
                &sha,
            );
            ctx
        };

        let mut ctx1 = build();
        let first = publish_to_krew(&mut ctx1, "widget", &quiet()).expect("first publish");
        assert!(first.pushed, "first publish must push the branch");

        // Second publish renders the identical manifest onto the same
        // branch — the remote tree already matches, so no push.
        let mut ctx2 = build();
        let second = publish_to_krew(&mut ctx2, "widget", &quiet()).expect("second publish");
        assert!(
            !second.pushed,
            "re-publishing an identical manifest must report NoChanges (pushed=false)"
        );
        drop(bare);
    }

    /// Workspace per-crate mode: each crate renders + pushes its OWN
    /// versioned branch under its OWN plugin name. Two krew crates sharing
    /// one bare fork must each land a distinct `plugins/<plugin>.yaml` on a
    /// distinct `<plugin>-v<version>` branch — proving the per-crate name +
    /// branch resolution is not clobbered by a sibling.
    #[test]
    #[serial(path_env)]
    fn publish_to_krew_pr_direct_workspace_per_crate_distinct_branches() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let alpha = pr_direct_crate("alpha", "kubectl-alpha", &bare_url);
        let beta = pr_direct_crate("beta", "kubectl-beta", &bare_url);
        let mut ctx = build_ctx(vec![alpha, beta], "2.3.4");
        inject_api_base(&mut ctx, &addr);
        for (cn, bin) in [("alpha", "kubectl-alpha"), ("beta", "kubectl-beta")] {
            add_archive(
                &mut ctx,
                cn,
                "x86_64-unknown-linux-gnu",
                "linux",
                "amd64",
                bin,
                &"f".repeat(64),
            );
        }

        publish_to_krew(&mut ctx, "alpha", &quiet()).expect("publish alpha");
        publish_to_krew(&mut ctx, "beta", &quiet()).expect("publish beta");

        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains("kubectl-alpha-v2.3.4"),
            "alpha branch missing; got:\n{branches}"
        );
        assert!(
            branches.contains("kubectl-beta-v2.3.4"),
            "beta branch missing; got:\n{branches}"
        );
        // Each branch carries only its own plugin manifest file.
        let alpha_file = git_stdout(
            bare.path(),
            &["show", "kubectl-alpha-v2.3.4:plugins/kubectl-alpha.yaml"],
        );
        assert!(alpha_file.contains("name: kubectl-alpha"), "{alpha_file}");
        let beta_file = git_stdout(
            bare.path(),
            &["show", "kubectl-beta-v2.3.4:plugins/kubectl-beta.yaml"],
        );
        assert!(beta_file.contains("name: kubectl-beta"), "{beta_file}");
        drop(bare);
    }

    /// `url_template` rewrites the pushed manifest's `platforms[].uri`
    /// (not the raw artifact URL). The landed manifest in the bare repo
    /// must carry the templated URL with `{{ name }}/{{ version }}/{{ os
    /// }}-{{ arch }}` substituted — proving the override survives the
    /// full render→push round-trip, not just an in-memory render.
    ///
    /// `{{ name }}` resolves to the CRATE name (`widget`), not the krew
    /// plugin-name override: `render_url_template_with_ctx` is called with
    /// `crate_name` as its `name` arg (krew.rs ~815). The crate here is
    /// `widget` and the plugin is `kubectl-widget`, so the two are
    /// distinguishable in the rendered uri.
    #[test]
    #[serial(path_env)]
    fn publish_to_krew_pr_direct_applies_url_template() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.url_template = Some(
                "https://dl.acme.example/{{ name }}/{{ version }}/{{ os }}-{{ arch }}.tar.gz"
                    .to_string(),
            );
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );

        publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
        let manifest_in_repo = git_stdout(
            bare.path(),
            &["show", "kubectl-widget-v1.0.0:plugins/kubectl-widget.yaml"],
        );
        assert!(
            manifest_in_repo
                .contains("uri: https://dl.acme.example/widget/1.0.0/linux-amd64.tar.gz"),
            "url_template must rewrite the pushed manifest uri ({{ name }} = \
             crate name 'widget'); got:\n{manifest_in_repo}"
        );
        // And the original (non-templated) artifact URL must be gone.
        assert!(
            !manifest_in_repo
                .contains("releases/download/v1.0.0/kubectl-widget-linux-amd64.tar.gz"),
            "the raw artifact URL must be replaced by the templated uri; got:\n{manifest_in_repo}"
        );
        drop(bare);
    }

    /// dry-run short-circuits before any clone/push: no branch lands in
    /// the bare repo, and the outcome reports `pushed = false`. Guards the
    /// "(dry-run) would submit …" early return from making real side
    /// effects.
    #[test]
    fn publish_to_krew_dry_run_makes_no_push() {
        let (bare_url, bare) = init_bare_fork();
        let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        let mut config = Config {
            crates: vec![c],
            ..Default::default()
        };
        config.project_name = "widget".to_string();
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("dry-run ok");
        assert!(!outcome.pushed, "dry-run must not push");
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("kubectl-widget-v1.0.0"),
            "dry-run must not push a branch; bare branches:\n{branches}"
        );
        drop(bare);
    }

    /// With no `krew.homepage` and no Cargo.toml `meta_homepage`, the
    /// manifest's `homepage:` falls back to the crate's `release.github`
    /// slug — `https://github.com/<owner>/<repo>`. Pins the GitHub-slug
    /// arm of the homepage-fallback chain (the crate's own repo, not the
    /// krew-index fork owner).
    #[test]
    fn render_manifest_homepage_falls_back_to_release_github_slug() {
        let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        // pr_direct_crate sets release.github = acme/widget and leaves
        // krew.homepage unset — exactly the GitHub-slug fallback case.
        let mut ctx = build_ctx(vec![c], "1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest.contains("homepage: https://github.com/acme/widget\n"),
            "homepage must derive from release.github slug; got:\n{manifest}"
        );
    }

    /// An explicit `krew.homepage` wins over the `release.github` slug
    /// fallback and is template-rendered (the `{{ .Version }}` here
    /// expands), so the operator override survives into the manifest.
    #[test]
    fn render_manifest_homepage_explicit_override_is_rendered() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.homepage = Some("https://docs.example/widget/{{ .Version }}".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest.contains("homepage: https://docs.example/widget/1.0.0\n"),
            "explicit homepage must win and render the template; got:\n{manifest}"
        );
        // The release.github slug must NOT be used for the homepage line —
        // the override fully replaces it. (The slug legitimately appears in
        // the artifact `uri:`, so assert on the `homepage:` line specifically
        // rather than a blanket substring.)
        assert!(
            !manifest.contains("homepage: https://github.com/acme/widget"),
            "the slug fallback must not drive the homepage; got:\n{manifest}"
        );
    }

    // -----------------------------------------------------------------
    // publish_to_krew — BotWebhook flow against a scripted responder.
    // -----------------------------------------------------------------

    /// `mode: bot` routes through the webhook flow: the publisher POSTs a
    /// `ReleaseRequest` (with the rendered manifest base64'd into
    /// `processedTemplate`) to the resolved webhook URL and, on HTTP 200,
    /// returns `pushed = false` (the bot owns the krew-index PR — nothing
    /// for anodizer to roll back). Asserts the request reached the
    /// responder carrying the plugin coordinates.
    #[test]
    fn publish_to_krew_bot_webhook_posts_release_request() {
        let (bare_url, bare) = init_bare_fork();
        let resp_body =
            "PR \"https://github.com/kubernetes-sigs/krew-index/pull/7\" submitted successfully";
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/github-action-webhook",
            response: Box::leak(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                )
                .into_boxed_str(),
            ),
            times: Some(1),
        }]);
        // The webhook URL is read from the env source on `ctx`
        // (`resolve_webhook_url(ctx.env_source())`), so point it at the
        // responder via the builder env (no process-env mutation needed).
        let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.mode = Some(KrewMode::Bot);
        }
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(anodizer_core::MapEnvSource::new().with(
            "KREW_RELEASE_BOT_WEBHOOK_URL",
            format!("http://{addr}/github-action-webhook"),
        ));
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );

        let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("webhook publish ok");
        assert!(
            !outcome.pushed,
            "BotWebhook flow must report pushed=false (bot owns the PR)"
        );
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one webhook POST expected");
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(payload["pluginName"], "kubectl-widget");
        assert_eq!(payload["tagName"], "v1.0.0");
        assert_eq!(payload["pluginOwner"], "acme");
        assert_eq!(payload["pluginRepo"], "widget");
        // The rendered manifest is base64'd into processedTemplate and
        // carries the crate's real artifact data.
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload["processedTemplate"].as_str().expect("base64 str"))
            .expect("decode");
        let manifest = String::from_utf8(decoded).expect("utf8");
        assert!(manifest.contains("name: kubectl-widget"), "{manifest}");
        assert!(manifest.contains("version: v1.0.0"), "{manifest}");
        drop(entries);
        drop(bare);
    }

    /// BotWebhook flow with no `release.github` owner/repo on the crate:
    /// the webhook needs the plugin's GitHub repo to identify the
    /// submission, so the publisher bails with an actionable error rather
    /// than POSTing a mis-targeted request.
    #[test]
    fn publish_to_krew_bot_webhook_without_release_github_bails() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        c.release = None; // No plugin GitHub coordinates.
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.mode = Some(KrewMode::Bot);
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        let err = publish_to_krew(&mut ctx, "widget", &quiet())
            .expect_err("webhook flow needs release.github");
        let msg = format!("{err:#}");
        assert!(msg.contains("release.github"), "got: {msg}");
        assert!(msg.contains("webhook"), "got: {msg}");
    }

    /// BotWebhook flow on a genuine server failure (HTTP 500 whose body is
    /// NOT an already-submitted signal): the publisher surfaces a loud
    /// error — krew must never silently skip a one-way publish.
    #[test]
    fn publish_to_krew_bot_webhook_genuine_failure_bails() {
        let (bare_url, bare) = init_bare_fork();
        let body = "opening pr: failed when validating plugin spec";
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/github-action-webhook",
            response: Box::leak(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .into_boxed_str(),
            ),
            times: Some(1),
        }]);
        let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.mode = Some(KrewMode::Bot);
        }
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(anodizer_core::MapEnvSource::new().with(
            "KREW_RELEASE_BOT_WEBHOOK_URL",
            format!("http://{addr}/github-action-webhook"),
        ));
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );
        let err = publish_to_krew(&mut ctx, "widget", &quiet()).expect_err("genuine 500 must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "got: {msg}");
        assert!(msg.contains("validating plugin spec"), "got: {msg}");
        drop(bare);
    }

    // -----------------------------------------------------------------
    // artifacts_to_platforms — multi-platform expansion the publish path
    // feeds into the manifest.
    // -----------------------------------------------------------------

    /// A multi-OS artifact set expands to one platform entry per OS with
    /// the correct krew os/arch labels and the `.exe` suffix on Windows —
    /// the shape the pushed manifest's `platforms[]` carries.
    #[test]
    fn artifacts_to_platforms_multi_os_labels_and_exe() {
        let arts = vec![
            make_os_artifact("linux", "amd64", Some("kubectl-widget")),
            make_os_artifact("darwin", "arm64", Some("kubectl-widget")),
            make_os_artifact("windows", "amd64", Some("kubectl-widget")),
        ];
        let plats = artifacts_to_platforms(&arts, "kubectl-widget");
        let find = |os: &str| plats.iter().find(|p| p.os == os).expect("platform");
        assert_eq!(find("linux").arch, "amd64");
        assert_eq!(find("linux").bin, "kubectl-widget");
        assert_eq!(find("darwin").arch, "arm64");
        assert_eq!(
            find("windows").bin,
            "kubectl-widget.exe",
            "windows bin must carry the .exe suffix krew needs"
        );
    }

    // -----------------------------------------------------------------
    // generate_manifest — empty optional narrative fields are dropped.
    // -----------------------------------------------------------------

    /// An empty `description` is serialized as absent (no `description:`
    /// key) — the `if params.description.is_empty()` → None branch. A
    /// blank `caveats` is likewise dropped, while `shortDescription`
    /// (always required) is still present.
    #[test]
    fn generate_manifest_empty_description_and_caveats_are_omitted() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "tool",
            version: "1.0.0",
            homepage: "https://example.com",
            short_description: "A tool",
            description: "",
            caveats: "",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/tool.tar.gz".to_string(),
                sha256: "hash".to_string(),
                bin: "kubectl-tool".to_string(),
                files: vec![],
            }],
        })
        .unwrap();
        assert!(
            !manifest.contains("description:"),
            "empty description must be omitted; got:\n{manifest}"
        );
        assert!(
            !manifest.contains("caveats:"),
            "empty caveats must be omitted; got:\n{manifest}"
        );
        assert!(
            manifest.contains("shortDescription: A tool"),
            "shortDescription is always present; got:\n{manifest}"
        );
    }

    // -----------------------------------------------------------------
    // publish_to_krew — skip / falsy-`if` short-circuits on the LIVE
    // publish path (distinct from the renderer's gates), returning the
    // skipped outcome before any repository resolution.
    // -----------------------------------------------------------------

    /// `skip: true` on the publish path returns a skipped outcome
    /// (pushed=false) BEFORE the missing-repository check fires — the
    /// crate here has no repository block, yet the call is `Ok`.
    #[test]
    fn publish_to_krew_skip_true_short_circuits_before_repo_check() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.repository = None;
            k.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let outcome = publish_to_krew(&mut ctx, "widget", &quiet())
            .expect("skip=true must short-circuit before the repo-missing check");
        assert!(!outcome.pushed, "skip path must report no push");
    }

    /// A falsy `if:` on the publish path returns a skipped outcome before
    /// the missing-repository check.
    #[test]
    fn publish_to_krew_falsy_if_short_circuits_before_repo_check() {
        let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
        if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
            k.repository = None;
            k.if_condition = Some("false".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        let outcome = publish_to_krew(&mut ctx, "widget", &quiet())
            .expect("falsy `if` must short-circuit before the repo-missing check");
        assert!(!outcome.pushed, "falsy `if` path must report no push");
    }

    // -----------------------------------------------------------------
    // KrewPublisher::run — real-push path records a rollback target.
    // -----------------------------------------------------------------

    /// Drive the Publisher trait's `run` end-to-end with a real PrDirect
    /// push against a local bare fork. The `any_pushed` gate must populate
    /// rollback evidence with exactly one target carrying the crate's
    /// upstream coordinates + the `{plugin}-v{version}` branch — proving
    /// `collect_krew_target` ran inside the per-crate scope.
    ///
    /// `run` re-scopes each crate's version through
    /// `with_published_crate_scope` → `resolve_crate_tag`, which hard-errors
    /// unless a real release tag matching the `v{{ .Version }}` template
    /// exists. `hermetic_tagged_repo()` (tag `v0.1.0`) supplies one, so the
    /// scoped version resolves deterministically to `0.1.0` and the branch
    /// is `<plugin>-v0.1.0`.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn krew_publisher_run_records_rollback_target_after_push() {
        use anodizer_core::Publisher;
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/krew-index/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        // Per-crate version resolution needs a real tag matching the
        // `v{{ .Version }}` template; the hermetic repo's `v0.1.0` supplies it.
        let project = crate::testing::hermetic_tagged_repo();
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                project_root: Some(project.path().to_path_buf()),
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "0.1.0");
        ctx.template_vars_mut().set("RawVersion", "0.1.0");
        ctx.template_vars_mut().set("Tag", "v0.1.0");
        inject_api_base(&mut ctx, &addr);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &"a".repeat(64),
        );

        let p = KrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        let targets = decode_krew_targets(&evidence.extra);
        assert_eq!(targets.len(), 1, "one pushed plugin → one rollback target");
        assert_eq!(targets[0].target, "widget");
        assert_eq!(targets[0].upstream_owner, "kubernetes-sigs");
        assert_eq!(targets[0].upstream_repo, "krew-index");
        assert_eq!(targets[0].fork_owner, "fork-owner");
        assert_eq!(
            targets[0].branch, "kubectl-widget-v0.1.0",
            "branch carries the per-crate-scoped version (v0.1.0 from the hermetic tag)"
        );
        drop(bare);
    }
}

// ---------------------------------------------------------------------------
// KrewPublisher — Publisher trait wrapper (close-PR rollback)
// ---------------------------------------------------------------------------

// Krew plugin-index publisher. Each successful per-crate publish opens a
// PR against an upstream `krew-index`-style repo from a fork. The rollback
// path closes those PRs via `PATCH /repos/<upstream>/pulls/<n>` with
// `state=closed`.
//
// PR-number discovery uses the query-at-rollback-time approach: at
// publish time only the upstream coordinates, the fork owner, and the
// branch name the publish path pushed to are recorded. At rollback time
// open PRs filtered by `head=<fork_owner>:<branch>` are listed and each
// match is closed. This sidesteps modifying the unchanged
// `publish_to_krew` body to surface the new PR number, and stays robust
// against a stale evidence file stitched in from an older run.
//
// CREDENTIAL HANDLING: `KrewPrTarget` stores `token_env_var` — the
// NAME of the env var to consult at rollback time — not the resolved
// token VALUE. Same rule applies to every PR-based publisher that
// touches GitHub auth.

simple_publisher!(
    KrewPublisher,
    "krew",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. One entry per crate whose publish path
/// successfully pushed a branch to its fork.
type KrewPrTarget = anodizer_core::publish_evidence::KrewTargetSnapshot;

/// Decode the `krew_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_krew_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<KrewPrTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Krew(k) => k.krew_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve the upstream `<owner>/<repo>` slug for a krew target — mirrors
/// the dispatch logic in `publish_to_krew`: prefer
/// `repository.pull_request.base` when set, else fall back to the
/// canonical kubernetes-sigs/krew-index.
fn resolve_krew_upstream(krew_cfg: &anodizer_core::config::KrewConfig) -> (String, String) {
    if let Some(base) = krew_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.base.as_ref())
        && let (Some(o), Some(n)) = (base.owner.as_deref(), base.name.as_deref())
    {
        return (o.to_string(), n.to_string());
    }
    ("kubernetes-sigs".to_string(), "krew-index".to_string())
}

/// Build a [`KrewPrTarget`] for each crate the publisher would run on.
/// Reads config + the live process version so the branch name matches
/// what `publish_to_krew` will push.
/// Snapshot the rollback PR target for a single crate under the version
/// currently scoped on `ctx`.
///
/// MUST be called inside the per-crate version scope so the recorded branch
/// (`{plugin}-v{version}`) matches the branch [`publish_to_krew`] actually
/// pushed — in workspace per-crate independent-version mode the global
/// `ctx.version()` is the FIRST crate's version, which would record the wrong
/// branch and orphan this crate's PR from rollback.
fn collect_krew_target(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<KrewPrTarget>> {
    let version = ctx.version();
    let Some(c) = crate::util::find_crate_in_universe(ctx, crate_name) else {
        return Ok(None);
    };
    let Some(krew_cfg) = c.publish.as_ref().and_then(|p| p.krew.as_ref()) else {
        return Ok(None);
    };
    let Some((fork_owner_raw, _)) =
        crate::util::resolve_repo_owner_name(krew_cfg.repository.as_ref())
    else {
        return Ok(None);
    };
    let fork_owner = util::render_or_warn(ctx, log, "krew.repository.owner", &fork_owner_raw)?;
    // Plugin-name override resolved through the same single-source helper
    // as `publish_to_krew` so the rollback-evidence branch name cannot
    // drift from the manifest `metadata.name` / file basename / webhook.
    let plugin_name = resolve_plugin_name(krew_cfg.name.as_deref(), &c.name, |t| {
        ctx.render_template(t)
    })?;
    let branch = format!("{}-v{}", plugin_name, version);
    let (upstream_owner, upstream_repo) = resolve_krew_upstream(krew_cfg);
    Ok(Some(KrewPrTarget {
        target: c.name.clone(),
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
        token_env_var: Some("KREW_INDEX_TOKEN".to_string()),
    }))
}

/// The crate-level `publish.krew` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::KrewConfig> {
    p.krew.as_ref()
}

pub(crate) fn is_krew_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Message emitted just before delegating to `publish_to_krew`. Anchors
/// the krew activity (plugin manifest render, fork clone, PR submission)
/// to a specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate krew publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_krew` on (not the
/// count of successful PRs — `publish_to_krew` has its own skip paths for
/// skip_upload/dry-run/etc., each of which logs its own status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished krew publish — {} configured crate(s) processed",
        processed
    )
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_krew_per_crate_configured`
/// check passed and whose `publish_to_krew` invocation was reached.
/// `selected_len` is the size of the implicit-all-resolved selection.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.krew` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.krew`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every krew-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "krew publisher registered but 0 of {} effective crate(s) had a krew \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.krew block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for KrewPublisher {
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
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Both krew flows need a token (PR-direct for the clone+PR, bot/auto
        // for the index probe + webhook), and the PR-direct flow clones
        // with git. `git` is declared unconditionally because `auto` mode
        // can resolve to PR-direct at run time.
        ctx.config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.krew.as_ref())
            .filter(|k| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    k.skip.as_ref(),
                    k.skip_upload.as_ref(),
                    k.if_condition.as_deref(),
                )
            })
            .flat_map(|k| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    k.repository.as_ref(),
                    Some("KREW_INDEX_TOKEN"),
                )
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_krew_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "krew",
            selected.len(),
        ));
        let mut processed = 0usize;
        let mut any_pushed = false;
        let mut targets: Vec<KrewPrTarget> = Vec::new();
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_krew_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("krew", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest — AND the rollback PR branch — carry the
            // crate's version, not the first crate's (workspace per-crate
            // independent-version mode). The target snapshot is collected inside
            // the same scope so its recorded branch matches the one pushed.
            let (pushed, target) = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let outcome = publish_to_krew(ctx, crate_name, &log)?;
                    let target = if outcome.pushed {
                        collect_krew_target(ctx, crate_name, &log)?
                    } else {
                        None
                    };
                    Ok((outcome.pushed, target))
                },
            )?;
            if pushed {
                any_pushed = true;
            }
            if let Some(t) = target {
                targets.push(t);
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("krew");
        // Record rollback evidence only for the PrDirect flow, which
        // pushes a branch + opens a PR anodizer can later close. The
        // BotWebhook flow has no anodizer-owned PR (the krew-release-bot
        // server opens it), so there is nothing to roll back and no
        // evidence to record.
        if any_pushed {
            evidence.extra = anodizer_core::PublishEvidenceExtra::Krew(
                anodizer_core::publish_evidence::KrewExtra {
                    krew_targets: targets,
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
        let targets = decode_krew_targets(&evidence.extra);

        // Only the PrDirect flow records PR targets; the BotWebhook flow
        // records none (the krew-release-bot server owns the PR). Nothing
        // to roll back when there are no targets.
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "krew",
                "PR targets",
            ));
            return Ok(());
        }

        // Resolve token at rollback time — never persisted in evidence.
        // Falls back to ANODIZER_GITHUB_TOKEN then GITHUB_TOKEN, same as
        // every git-revert publisher.
        let env = ctx.env_source();
        let resolve_token = |t: &KrewPrTarget| -> Option<String> {
            util::resolve_rollback_token(env, t.token_env_var.as_deref())
        };

        // Fan out at PR granularity, not target granularity: a single
        // krew target can map to multiple open PRs if the publish path
        // pushed the same branch twice (idempotent re-publish). We dedup
        // PR numbers per (upstream, n) so we don't try to close the same
        // PR twice when two targets share the same fork branch.
        struct CloseJob {
            upstream_owner: String,
            upstream_repo: String,
            pr_number: u64,
            token: String,
            target_label: String,
        }
        let mut jobs: Vec<CloseJob> = Vec::new();
        let mut seen: std::collections::BTreeSet<(String, String, u64)> =
            std::collections::BTreeSet::new();
        for t in &targets {
            let Some(token) = resolve_token(t) else {
                log.warn(&format!(
                    "skipped rollback for {} — no krew token resolvable (env var ${} / \
                     {} all unset)",
                    t.target,
                    t.token_env_var.as_deref().unwrap_or("KREW_INDEX_TOKEN"),
                    anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / "),
                ));
                continue;
            };
            let env_hint_for_target = t.token_env_var.as_deref().unwrap_or("KREW_INDEX_TOKEN");
            let pr_numbers = match crate::util::find_open_pr_numbers_for_head(
                &t.upstream_owner,
                &t.upstream_repo,
                &t.fork_owner,
                &t.branch,
                Some(&token),
                env_hint_for_target,
            ) {
                Ok(v) => v,
                Err(e) => {
                    // Auth-failure / repo-not-found / transport problems
                    // surface as actionable warns naming the actual
                    // failure mode — not the misleading "no PR found,
                    // verify manually" that previously fired here.
                    log.warn(&format!(
                        "failed to query krew upstream {}/{} for open PRs ({}); \
                         {} — manual cleanup required",
                        t.upstream_owner, t.upstream_repo, t.target, e
                    ));
                    continue;
                }
            };
            if pr_numbers.is_empty() {
                log.warn(&format!(
                    "no open krew PRs found for head={}:{} against {}/{}; \
                     verify manually",
                    t.fork_owner, t.branch, t.upstream_owner, t.upstream_repo,
                ));
                continue;
            }
            for n in pr_numbers {
                let key = (t.upstream_owner.clone(), t.upstream_repo.clone(), n);
                if seen.insert(key) {
                    jobs.push(CloseJob {
                        upstream_owner: t.upstream_owner.clone(),
                        upstream_repo: t.upstream_repo.clone(),
                        pr_number: n,
                        token: token.clone(),
                        target_label: t.target.clone(),
                    });
                }
            }
        }

        let env_hint = targets
            .first()
            .and_then(|t| t.token_env_var.as_deref())
            .unwrap_or("KREW_INDEX_TOKEN");

        // Three-bucket count: (closed, already_closed, failed).
        // `already_closed` is a success bucket — 404 / 410 / 422 from
        // the PATCH means the desired end-state ("PR not open") is
        // already true (maintainer closed it, repo renamed, PR
        // deleted). Re-running --rollback-only after a partial
        // success must NOT surface those as failures.
        let counts = std::sync::Mutex::new((0usize, 0usize, 0usize));
        for chunk in jobs.chunks(crate::util::ROLLBACK_PARALLELISM) {
            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(chunk.len());
                for job in chunk {
                    let log = log.clone();
                    let counts = &counts;
                    handles.push(s.spawn(move || {
                        let pr_url = format!(
                            "https://github.com/{}/{}/pull/{}",
                            job.upstream_owner, job.upstream_repo, job.pr_number
                        );
                        log.status(&format!(
                            "closing krew PR {} ({})",
                            job.target_label, pr_url
                        ));
                        let outcome = crate::util::close_pr_via_api(
                            &job.upstream_owner,
                            &job.upstream_repo,
                            job.pr_number,
                            &job.token,
                        );
                        match outcome {
                            crate::util::CloseOutcome::Closed => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.0 += 1;
                            }
                            crate::util::CloseOutcome::AlreadyClosed => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.1 += 1;
                                log.status(&format!(
                                    "krew PR {} ({}) already closed/deleted upstream — \
                                     rollback noticed the existing state",
                                    job.target_label, pr_url
                                ));
                            }
                            crate::util::CloseOutcome::Failed(err) => {
                                let mut c = crate::util::lock_recover(counts, &log, "krew");
                                c.2 += 1;
                                log.warn(&crate::publisher_helpers::rollback_failure_warning_msg(
                                    "krew",
                                    &job.target_label,
                                    &pr_url,
                                    &err,
                                    Some(env_hint),
                                ));
                            }
                        }
                    }));
                }
                for h in handles {
                    crate::util::join_or_warn(h, &log, "krew");
                }
            });
        }
        // `into_inner` consumes the Mutex; poison here means a worker
        // panicked. Counter state is still valid (3-tuple of usize) so
        // recover and emit the summary rather than abandon the operator.
        let (closed, already_closed, failed) = match counts.into_inner() {
            Ok(c) => c,
            Err(poisoned) => {
                log.warn("krew mutex poisoned by worker panic; reporting counters as-of poison");
                poisoned.into_inner()
            }
        };
        log.status(&format!(
            "krew rollback closed {}, already-closed {}, failed {}",
            closed, already_closed, failed
        ));
        Ok(())
    }

    /// Probe every active krew-index fork for existence + push scope before any
    /// publisher runs: a missing fork or a token that cannot open the PR fails
    /// the PR-direct flow after sibling publishers may already have shipped.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "KREW_INDEX_TOKEN",
            ctx.config
                .crate_universe()
                .into_iter()
                .filter_map(|c| c.publish.as_ref().and_then(|p| p.krew.as_ref())),
            |k| {
                // Krew carries a `skip` field, unlike scoop/homebrew/winget.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    k.skip.as_ref(),
                    k.skip_upload.as_ref(),
                    k.if_condition.as_deref(),
                )
            },
            |k| k.repository.as_ref(),
        ))
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{
        CrateConfig, KrewConfig, PublishConfig, RepositoryConfig, StringOrBool,
    };
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn krew_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("krew-index-fork".to_string()),
                        ..Default::default()
                    }),
                    short_description: Some("a kubectl plugin".to_string()),
                    description: Some("a kubectl plugin".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn krew_publisher_classification() {
        let p = KrewPublisher::new();
        assert_eq!(p.name(), "krew");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN pull_request:write")
        );
    }

    #[test]
    fn krew_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = KrewPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn krew_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("krew");
        let p = KrewPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("krew") && m.contains("PR targets") && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Rollback with a recorded target but NO token resolvable from the
    /// env: the per-target loop must warn (naming the target + the env
    /// var it tried) and `continue` WITHOUT making any network call —
    /// then complete `Ok(())`, emitting the all-zero summary. Pins the
    /// no-token skip arm that protects against firing a credential-less
    /// GitHub API request.
    #[test]
    fn krew_rollback_warns_and_skips_target_when_no_token_resolvable() {
        let capture = anodizer_core::log::LogCapture::new();
        // A sealed (closed, empty) env source carries NONE of
        // KREW_INDEX_TOKEN / ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN, so
        // resolve_token yields None and the target is skipped before any
        // api.github.com request.
        let mut ctx = TestContextBuilder::new().sealed_env().build();
        ctx.with_log_capture(capture.clone());
        let mut evidence = PublishEvidence::new("krew");
        evidence.extra =
            anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
                krew_targets: vec![KrewPrTarget {
                    target: "demo".into(),
                    upstream_owner: "kubernetes-sigs".into(),
                    upstream_repo: "krew-index".into(),
                    fork_owner: "acme".into(),
                    branch: "demo-v1.2.3".into(),
                    token_env_var: Some("KREW_INDEX_TOKEN".into()),
                }],
            });
        let p = KrewPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("no krew token resolvable")
                && m.contains("demo")
                && m.contains("KREW_INDEX_TOKEN")),
            "expected a no-token warn naming the target + env var; got: {warns:?}"
        );
        // The final summary reports zero work — no PR was queried or closed.
        let all = capture.all_messages();
        assert!(
            all.iter().any(|(_, m)| m.contains("closed 0")
                && m.contains("already-closed 0")
                && m.contains("failed 0")),
            "no-token skip must leave all counters at zero; got: {all:?}"
        );
    }

    #[test]
    fn krew_target_extra_roundtrips() {
        let original = vec![KrewPrTarget {
            target: "demo".into(),
            upstream_owner: "kubernetes-sigs".into(),
            upstream_repo: "krew-index".into(),
            fork_owner: "acme".into(),
            branch: "demo-v1.2.3".into(),
            token_env_var: Some("KREW_INDEX_TOKEN".into()),
        }];
        let extra =
            anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
                krew_targets: original.clone(),
            });
        let decoded = decode_krew_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn krew_target_extra_carries_no_secret_material() {
        // Structural pin: build a typed-variant evidence and assert
        // (a) no credential-shaped keys appear AND (b) the
        // operator-public PR coordinates are preserved.
        let mut e = anodizer_core::PublishEvidence::new("krew");
        e.extra =
            anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
                krew_targets: vec![KrewPrTarget {
                    target: "demo".into(),
                    upstream_owner: "kubernetes-sigs".into(),
                    upstream_repo: "krew-index".into(),
                    fork_owner: "acme".into(),
                    branch: "demo-v1.2.3".into(),
                    token_env_var: Some("KREW_INDEX_TOKEN".into()),
                }],
            });
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(s.contains("KREW_INDEX_TOKEN"), "{s}");
        assert!(s.contains("\"upstream_owner\":\"kubernetes-sigs\""), "{s}");
        assert!(s.contains("\"upstream_repo\":\"krew-index\""), "{s}");
        assert!(s.contains("\"fork_owner\":\"acme\""), "{s}");
    }

    #[test]
    fn krew_effective_publish_crates_implicit_all_when_selection_empty() {
        // Regression pin for the `selected_crates = Vec::new()` failure
        // mode: the run path used to iterate the empty Vec and silently
        // skip every configured krew plugin. The helper now resolves to
        // implicit-all over `publish.krew`-carrying crates.
        let ctx = TestContextBuilder::new()
            .crates(vec![
                krew_crate("alpha"),
                krew_crate("beta"),
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_krew_per_crate_configured);
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn krew_effective_publish_crates_honors_non_empty_selection() {
        let ctx = TestContextBuilder::new()
            .crates(vec![krew_crate("alpha"), krew_crate("beta")])
            .selected_crates(vec!["beta".to_string()])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_krew_per_crate_configured);
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn krew_collect_run_targets_uses_default_upstream() {
        let ctx = TestContextBuilder::new()
            .crates(vec![krew_crate("demo")])
            .build();
        let target = collect_krew_target(&ctx, "demo", &ctx.logger("publish"))
            .expect("render ok")
            .expect("target");
        assert_eq!(target.target, "demo");
        assert_eq!(target.upstream_owner, "kubernetes-sigs");
        assert_eq!(target.upstream_repo, "krew-index");
        assert_eq!(target.fork_owner, "acme");
        assert!(
            target.branch.starts_with("demo-v"),
            "branch: {}",
            target.branch
        );
    }

    #[test]
    fn krew_collect_run_targets_honors_pull_request_base_override() {
        use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig};
        let mut c = krew_crate("demo");
        if let Some(p) = c.publish.as_mut()
            && let Some(k) = p.krew.as_mut()
            && let Some(r) = k.repository.as_mut()
        {
            r.pull_request = Some(PullRequestConfig {
                enabled: Some(true),
                base: Some(PullRequestBaseConfig {
                    owner: Some("custom-org".to_string()),
                    name: Some("custom-index".to_string()),
                    branch: None,
                }),
                draft: None,
                body: None,
            });
        }
        let ctx = TestContextBuilder::new().crates(vec![c]).build();
        let target = collect_krew_target(&ctx, "demo", &ctx.logger("publish"))
            .expect("render ok")
            .expect("target");
        assert_eq!(target.upstream_owner, "custom-org");
        assert_eq!(target.upstream_repo, "custom-index");
    }

    // -----------------------------------------------------------------------
    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary.

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("starting per-crate krew publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished krew publish"), "{msg}");
        assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("krew publisher registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// The no-eligible-crates warning must fire only when the iteration
    /// loop's configured-predicate filtered every selected crate out — not
    /// when the publish path was reached successfully.
    #[test]
    fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
        // One configured crate reached the publish path → no warning.
        assert!(!should_warn_no_eligible(1, 1));
        // True positive: none configured.
        assert!(should_warn_no_eligible(0, 3));
        // Empty selection → no warning.
        assert!(!should_warn_no_eligible(0, 0));
        // Partial-skip → no warning.
        assert!(!should_warn_no_eligible(1, 3));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects a krew-configured crate. Verifies the run path is wired
    /// (returns Ok). The log lines are written to stderr and asserted
    /// indirectly via the helper-string tests above.
    #[test]
    fn krew_publisher_run_dry_run_returns_ok() {
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![krew_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = KrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        // dry-run publish_to_krew short-circuits before branch push; no actual
        // push occurred so evidence.extra must be empty (no phantom targets).
        let targets = decode_krew_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run must not record rollback targets: {targets:?}"
        );
    }

    /// When the publisher is registered (a crate has a krew block) but the
    /// selected-crates filter excludes every krew-configured crate, the run
    /// path must still return Ok and the processed count is zero.
    #[test]
    fn krew_publisher_run_no_eligible_crates_returns_ok() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                krew_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-krew crate — publisher registered but
            // run path will iterate zero krew-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = KrewPublisher::new();
        // Must return Ok even when no krew-configured crate is selected.
        p.run(&mut ctx).expect("publisher.run ok");
    }

    /// Implicit-all selection (empty `selected_crates`) + dry-run must
    /// produce empty evidence. The implicit-all path resolves through
    /// `effective_publish_crates` to every krew-configured crate, so this
    /// pins the gate where phantom rollback targets used to leak.
    #[test]
    fn test_publish_to_krew_dry_run_implicit_all_produces_empty_evidence() {
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![krew_crate("demo"), krew_crate("other")])
            // No selected_crates → implicit-all resolves to both krew crates.
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = KrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run implicit-all publisher.run");
        let targets = decode_krew_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run + implicit-all must not record rollback targets: {targets:?}"
        );
    }

    /// skip_upload path must produce empty evidence — no branch push occurred.
    #[test]
    fn krew_publisher_run_skip_upload_produces_empty_evidence() {
        let mut crate_with_skip = krew_crate("demo");
        if let Some(ref mut publish) = crate_with_skip.publish
            && let Some(ref mut krew) = publish.krew
        {
            krew.skip_upload = Some(StringOrBool::Bool(true));
        }
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with_skip])
            .selected_crates(vec!["demo".to_string()])
            .project_root(repo.path().to_path_buf())
            .build();
        let p = KrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("skip_upload publisher.run");
        let targets = decode_krew_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "skip_upload must not record rollback targets: {targets:?}"
        );
    }

    #[test]
    fn krew_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![krew_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = KrewPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }

    /// Building a krew plugin manifest for an artifact whose `sha256`
    /// metadata is empty must bail with an actionable error. Defaulting
    /// to `""` would embed an empty `sha256:` field in the rendered
    /// manifest, which krew's `addURIAndSha` validator rejects at
    /// install time. The bail message must name the publisher, the
    /// field, the offending artifact context, and a next-step hint.
    #[test]
    fn krew_sha256_empty_metadata_bails_with_actionable_error() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{
            Config, CrateConfig, KrewConfig, PublishConfig, RepositoryConfig,
        };
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};
        let config = Config {
            crates: vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    krew: Some(KrewConfig {
                        repository: Some(RepositoryConfig {
                            owner: Some("acme".to_string()),
                            name: Some("krew-index-fork".to_string()),
                            ..Default::default()
                        }),
                        short_description: Some("a kubectl plugin".to_string()),
                        description: Some("a kubectl plugin".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/mytool-linux-amd64.tar.gz"),
            name: "mytool-linux-amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mytool".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "url".to_string(),
                    "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
                );
                m.insert("extra_binaries".to_string(), "mytool".to_string());
                m
            },
            size: None,
        });
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err = publish_to_krew(&mut ctx, "mytool", &log).expect_err("missing sha256 must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing sha256 metadata"),
            "error must mention missing sha256; got: {msg}"
        );
        assert!(
            msg.contains("mytool"),
            "error must name the offending crate; got: {msg}"
        );
        assert!(
            msg.contains("checksum stage"),
            "error must mention the checksum stage; got: {msg}"
        );
    }

    /// The krew `short_description` gate intentionally bails only when BOTH
    /// `short_description` AND the effective description (including the
    /// Cargo.toml-derived one) are empty. A crate with no `short_description`
    /// but a Cargo.toml `package.description` must get PAST the gate (and the
    /// short_description fall back to that description), failing later on the
    /// missing artifact/sha256 — never on "short_description is not set".
    #[test]
    fn krew_short_description_falls_back_to_cargo_toml_description() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("mytool");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"mytool\"\ndescription = \"a derived kubectl plugin\"\n",
        )
        .unwrap();

        let mut config = Config {
            crates: vec![CrateConfig {
                name: "mytool".to_string(),
                path: "mytool".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    krew: Some(KrewConfig {
                        repository: Some(RepositoryConfig {
                            owner: Some("acme".to_string()),
                            name: Some("krew-index-fork".to_string()),
                            ..Default::default()
                        }),
                        // No short_description AND no description here — both
                        // must come from the crate's Cargo.toml.
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.populate_derived_metadata(tmp.path());

        let mut ctx = Context::new(config, ContextOptions::default());
        let log = StageLogger::new("publish", Verbosity::Quiet);
        // No artifacts registered → fails downstream, but NOT on the gate.
        let err = publish_to_krew(&mut ctx, "mytool", &log)
            .expect_err("no artifacts → must still fail downstream");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("short_description is not set"),
            "short_description must fall back to Cargo.toml description, not gate-bail; got: {msg}"
        );
        assert!(
            !msg.contains("description is not set"),
            "description must resolve from Cargo.toml; got: {msg}"
        );
    }
}
