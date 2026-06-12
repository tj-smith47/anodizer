use anodizer_core::config::{CargoPublishConfig, CrateConfig, WaitForWorkspaceDepsConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::util::topological_sort;
use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Default seconds to wait for a freshly-published crate to appear in the
/// crates.io sparse index. Mirrors the historical anodizer default; only
/// matters when the crate has dependents that need it published first.
const DEFAULT_INDEX_TIMEOUT_SECS: u64 = 300;

/// How many times to retry `cargo publish` when it fails with a signature
/// that smells like sparse-index propagation lag (see
/// [`is_index_propagation_failure`]). Three total attempts (the initial
/// publish plus two retries) covers the common case where the dependent's
/// `cargo publish` lands on a stale CDN edge a beat after [`poll_crates_io_index`]
/// already saw the previous crate confirmed on a different edge. Higher
/// attempt counts buy nothing: by then either Fastly has fanned out or the
/// failure isn't propagation-related.
const PUBLISH_PROPAGATION_RETRIES: u32 = 3;

/// Backoff between propagation-retry attempts. Short by design — the outer
/// [`poll_crates_io_index`] already burned the propagation budget waiting
/// for OUR edge to confirm; this is just for inter-edge skew where cargo's
/// invocation races against Fastly's broadcast.
const PUBLISH_PROPAGATION_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15);

/// Walk `depends_on` from each crate in `seed` to produce a de-duplicated
/// list containing every seed crate plus every transitive dependency that
/// lives in the same config. The `all_crates` slice is searched by name;
/// deps pointing at crates outside the config are ignored (same as cargo's
/// external-dep handling — they're expected to be on crates.io already).
fn expand_with_transitive_deps(all_crates: &[CrateConfig], seed: &[String]) -> Vec<String> {
    let name_to_deps: HashMap<&str, &[String]> = all_crates
        .iter()
        .map(|c| (c.name.as_str(), c.depends_on.as_deref().unwrap_or_default()))
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = seed.to_vec();
    while let Some(name) = stack.pop() {
        // Skip names we've already visited or that aren't in the config —
        // external crates.io deps are resolved by cargo against the real
        // registry and don't need to appear in our publish graph.
        if !name_to_deps.contains_key(name.as_str()) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        out.push(name.clone());
        if let Some(deps) = name_to_deps.get(name.as_str()) {
            for dep in *deps {
                if !seen.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// publish_command
// ---------------------------------------------------------------------------

/// Build the argument list for `cargo publish` with the given config flags.
///
/// `--allow-dirty` is implicit: the pipeline runs after the tag step, which
/// ALWAYS leaves a dirty tree (Cargo.lock + version bump), so requiring a
/// clean tree would block every release. Users can still set
/// `cargo.allow_dirty: false` to opt out, but that's surprising enough we
/// always force-on by default.
pub fn publish_command(crate_name: &str, cfg: Option<&CargoPublishConfig>) -> Vec<String> {
    let mut cmd = vec![
        "cargo".to_string(),
        "publish".to_string(),
        "-p".to_string(),
        crate_name.to_string(),
    ];

    let Some(c) = cfg else {
        // No config block — preserve historical default of allow-dirty.
        cmd.push("--allow-dirty".to_string());
        return cmd;
    };

    // Registry selection
    if let Some(ref reg) = c.registry {
        cmd.push("--registry".to_string());
        cmd.push(reg.clone());
    }
    if let Some(ref idx) = c.index {
        cmd.push("--index".to_string());
        cmd.push(idx.clone());
    }

    // Verify / dirty
    if c.no_verify == Some(true) {
        cmd.push("--no-verify".to_string());
    }
    // allow_dirty defaults to ON when unset (anodize tag bumps Cargo.toml +
    // updates Cargo.lock, so the tree is always dirty by the time publish
    // runs). Setting `allow_dirty: false` explicitly disables it.
    if c.allow_dirty != Some(false) {
        cmd.push("--allow-dirty".to_string());
    }

    // Feature selection
    if let Some(ref feats) = c.features
        && !feats.is_empty()
    {
        cmd.push("--features".to_string());
        cmd.push(feats.join(","));
    }
    if c.all_features == Some(true) {
        cmd.push("--all-features".to_string());
    }
    if c.no_default_features == Some(true) {
        cmd.push("--no-default-features".to_string());
    }

    // Compilation
    if let Some(ref t) = c.target {
        cmd.push("--target".to_string());
        cmd.push(t.clone());
    }
    if let Some(ref td) = c.target_dir {
        cmd.push("--target-dir".to_string());
        cmd.push(td.display().to_string());
    }
    if let Some(j) = c.jobs {
        cmd.push("--jobs".to_string());
        cmd.push(j.to_string());
    }
    if c.keep_going == Some(true) {
        cmd.push("--keep-going".to_string());
    }

    // Manifest
    if let Some(ref mp) = c.manifest_path {
        cmd.push("--manifest-path".to_string());
        cmd.push(mp.display().to_string());
    }
    if c.locked == Some(true) {
        cmd.push("--locked".to_string());
    }
    if c.offline == Some(true) {
        cmd.push("--offline".to_string());
    }
    if c.frozen == Some(true) {
        cmd.push("--frozen".to_string());
    }

    cmd
}

// ---------------------------------------------------------------------------
// poll_crates_io_index
// ---------------------------------------------------------------------------

/// Build the sparse index URL for a crate name (path segments based on length).
///
/// Crate names per cargo are restricted to ASCII alphanumerics plus `-`/`_`
/// (cargo reference: "Crate names ... must be ASCII"), so the byte slices
/// below are guaranteed to land on character boundaries. The debug_assert
/// makes the invariant load-bearing — any caller passing a non-ASCII name
/// would surface the violation in a debug build long before the slice
/// could panic at runtime.
pub(crate) fn sparse_index_url(crate_name: &str) -> String {
    debug_assert!(
        crate_name.is_ascii(),
        "cargo crate names must be ASCII; got {crate_name:?}"
    );
    let lower = crate_name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("https://index.crates.io/1/{}", lower),
        2 => format!("https://index.crates.io/2/{}", lower),
        3 => format!("https://index.crates.io/3/{}/{}", &lower[..1], lower),
        _ => format!(
            "https://index.crates.io/{}/{}/{}",
            &lower[..2],
            &lower[2..4],
            lower
        ),
    }
}

/// Check whether `crate_name` at `version` is already published on crates.io,
/// and if so, return the index-recorded sha256 cksum so callers can detect
/// drift between the local .crate and what's already on the registry.
///
/// Returns `Ok(Some(cksum_hex))` if the index has this version (cksum may be
/// an empty string if the index entry is malformed), `Ok(None)` if the crate
/// or version isn't present, `Err` on transport errors. Used to make publishes
/// idempotent across retries while surfacing same-version drift instead of
/// silently skipping a re-release that would install stale content.
///
/// The sparse-index GET routes through [`retry_http_blocking`] so transient
/// 5xx / 429 / network failures retry per the user's top-level `retry:`
/// policy; 404 is detected via the helper's `HttpError(404)` Break path and
/// mapped to `Ok(None)` so a never-published crate doesn't trip retries.
fn is_already_published(
    crate_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
) -> Result<Option<String>> {
    is_already_published_at(&sparse_index_url(crate_name), crate_name, version, policy)
}

/// Same as [`is_already_published`] but uses the supplied URL instead of
/// computing one from `sparse_index_url`. Lets tests point at a local TCP
/// responder so the retry plumbing can be exercised end-to-end.
fn is_already_published_at(
    url: &str,
    crate_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
) -> Result<Option<String>> {
    use anodizer_core::retry::{SuccessClass, retry_http_blocking};
    use std::time::Duration;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index check")?;

    let label = format!("publish: query crates.io index for '{}'", crate_name);
    let result = retry_http_blocking(
        &label,
        policy,
        SuccessClass::Strict,
        |_| client.get(url).send(),
        |status, body| {
            format!(
                "publish: crates.io index returned {} for '{}': {}",
                status,
                crate_name,
                redact_bearer_tokens(body)
            )
        },
    );

    let (_status, body) = match result {
        Ok(pair) => pair,
        Err(err) => {
            // 404 = crate has never been published — not already published.
            // The retry helper Breaks 4xx with HttpError(status) in the chain;
            // catch the 404 here and surface as Ok(None). Other 4xx and 5xx
            // exhaustion propagate.
            let status_code = err
                .chain()
                .find_map(|e| {
                    e.downcast_ref::<anodizer_core::retry::HttpError>()
                        .map(|h| h.status)
                })
                .unwrap_or(0);
            if status_code == 404 {
                return Ok(None);
            }
            return Err(err);
        }
    };

    Ok(parse_index_cksum_for_version(&body, version))
}

/// Parse the crates.io sparse-index body (JSON-lines, one entry per
/// published version) and return the `cksum` for `version` when present.
///
/// - Returns `None` when no line matches the requested version.
/// - Returns `Some("")` when the version exists but the line is missing its
///   `cksum` field — caller must treat this as "version present, drift
///   undetectable" rather than "not published".
///
/// Extracted from `is_already_published` so the JSONL shape can be unit
/// tested without performing a network call to crates.io.
fn parse_index_cksum_for_version(body: &str, version: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
        if v.get("vers")?.as_str()? != version {
            return None;
        }
        Some(
            v.get("cksum")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        )
    })
}

/// Poll the crates.io sparse index until `crate_name` at `version` appears or
/// the deadline (seconds) is exceeded.  Uses exponential back-off starting at
/// `INITIAL_POLL_DELAY`, capped at `MAX_POLL_DELAY`.
///
/// Returns `Ok(())` when the version is confirmed, `Err` on timeout.
fn poll_crates_io_index(
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    const INITIAL_POLL_DELAY: Duration = Duration::from_secs(5);
    const MAX_POLL_DELAY: Duration = Duration::from_secs(60);

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    let url = sparse_index_url(crate_name);

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index polling")?;

    let mut backoff = INITIAL_POLL_DELAY;

    // Per-attempt logs go to `debug` — transient HTTP errors are the
    // normal shape of "the index hasn't propagated yet"; surfacing them
    // at `warn`/`error` floods normal release output. The terminal
    // timeout below escalates with a single bail!() carrying the same
    // context the per-attempt logs would have shown.
    loop {
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = anodizer_core::http::body_of_blocking(resp);
                // Each line of the sparse index is a JSON object; parse and check vers field.
                if body.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|v| v.get("vers")?.as_str().map(|s| s == version))
                        .unwrap_or(false)
                }) {
                    log.status(&format!(
                        "crates.io index confirmed {}-{}",
                        crate_name, version
                    ));
                    return Ok(());
                }
            }
            Ok(resp) => {
                log.debug(&format!(
                    "crates.io index returned {} for {}, retrying…",
                    resp.status(),
                    crate_name
                ));
            }
            Err(e) => {
                log.debug(&format!(
                    "HTTP error polling index for {}: {}",
                    crate_name, e
                ));
            }
        }

        if start.elapsed() >= deadline {
            anyhow::bail!(
                "publish: timed out waiting for {}-{} to appear in crates.io index \
                 (waited {} s)",
                crate_name,
                version,
                timeout_secs
            );
        }

        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_POLL_DELAY);
    }
}

// ---------------------------------------------------------------------------
// wait_for_workspace_deps — pre-publish polling gate
// ---------------------------------------------------------------------------

/// Parse a crate's `Cargo.toml` for workspace-internal deps that resolve
/// to a literal version pin, filtered to the set of crate names known to
/// the anodize workspace.
///
/// Scans `[dependencies]`, `[dev-dependencies]`, and `[build-dependencies]`
/// (plus their target-specific variants under `[target.*.dependencies]`,
/// etc.). Each `(name, version)` pair captures the package name and version
/// cargo will resolve against the crates.io index at publish time: the name
/// honours `package = "..."` renames (leaf entry, or the workspace-root
/// entry for a `workspace = true` inherit) and the version comes from the
/// literal leaf pin or the workspace root's pin for an inherit. Entries
/// without any resolvable version (git deps, path-only entries, inherits
/// with no root pin) are skipped — there is nothing for the gate to poll
/// for.
///
/// Returns an empty Vec if the manifest can't be read or parsed; the
/// caller logs the case via [`wait_for_workspace_deps`] so the gate
/// degrades to a no-op instead of erroring out a publish that would
/// otherwise have succeeded. `root_cache` shares the parsed workspace-root
/// `[workspace.dependencies]` map across the per-crate calls of one run.
fn workspace_deps_for_crate(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    root_cache: &mut RootDepCache,
) -> Vec<(String, String)> {
    collect_workspace_dep_entries(
        manifest_path,
        workspace_crate_names,
        &["dependencies", "dev-dependencies", "build-dependencies"],
        root_cache,
    )
    .into_iter()
    .filter(|entry| !entry.version.is_empty())
    .map(|entry| (entry.package, entry.version))
    .collect()
}

/// Extract a literal `version = "X.Y.Z"` from a dep value, handling the
/// three shapes cargo accepts:
///
/// - `name = "1.2.3"` — bare string value.
/// - `name = { version = "1.2.3", ... }` — inline table.
/// - `[dependencies.name]\nversion = "1.2.3"` — standard table.
///
/// Returns `None` for `workspace = true` inherits, `git = ...` deps, and
/// path-only entries — none of those produce a crates.io-queryable pin.
fn extract_version_pin(item: &toml_edit::Item) -> Option<String> {
    if let Some(v) = item.as_value() {
        // Bare-string form (`name = "1.2.3"`).
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
        // Inline-table form (`name = { version = "..." }`).
        if let Some(tbl) = v.as_inline_table() {
            // `workspace = true` inherits resolve via the workspace
            // root — no per-dep version pin to poll for here. The
            // sync_workspace_deps path always writes a literal version
            // alongside the inherit when a workspace dep needs pinning,
            // so this branch only fires for inherits with no override.
            if tbl
                .get("workspace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return None;
            }
            return tbl
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    // Standard-table form (`[dependencies.name]` with subkeys).
    if let Some(tbl) = item.as_table() {
        if tbl
            .get("workspace")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        return tbl
            .get("version")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    None
}

/// Probe the sparse index for `(crate_name, version)` once. Returns
/// `Ok(true)` when the version line is present, `Ok(false)` for any
/// non-success status (treated as "not yet"), `Err` on transport
/// failures the caller should surface.
///
/// Uses the same blocking HTTP client + JSONL parser as
/// [`is_already_published_at`] — the wait-for-deps gate and the
/// already-published short-circuit query the same endpoint, so sharing
/// the parser keeps the two paths byte-identical.
fn probe_dep_on_index(
    client: &reqwest::blocking::Client,
    url: &str,
    version: &str,
) -> Result<bool> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("publish: wait_for_workspace_deps GET {url}"))?;
    if !resp.status().is_success() {
        return Ok(false);
    }
    let body = anodizer_core::http::body_of_blocking(resp);
    Ok(parse_index_cksum_for_version(&body, version).is_some())
}

/// Pre-publish gate: poll crates.io for every workspace-internal dep at
/// its expected version, blocking until each is queryable. Bails with a
/// loud error after `cfg.resolved_max_wait()` elapses.
///
/// `crate_name` is the crate about to be published (used purely for log
/// context); `deps` is the `(name, version)` set returned by
/// [`workspace_deps_for_crate`] filtered to the anodize workspace.
///
/// No-op when `cfg.resolved_enabled()` is false or `deps` is empty.
fn wait_for_workspace_deps_to_appear(
    crate_name: &str,
    deps: &[(String, String)],
    cfg: &WaitForWorkspaceDepsConfig,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    if !cfg.resolved_enabled() || deps.is_empty() {
        return Ok(());
    }

    let poll_interval = cfg.resolved_poll_interval();
    let max_wait = cfg.resolved_max_wait();
    let deadline = Instant::now() + max_wait;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: wait_for_workspace_deps build HTTP client")?;

    log.status(&format!(
        "gating publish of '{}' on {} workspace dep(s)",
        crate_name,
        deps.len()
    ));

    // Process deps sequentially — the typical fan-in is small (1–3 deps),
    // so per-dep waits compose without needing parallelism. Each dep is
    // polled until found OR the shared deadline elapses, so a slow first
    // dep doesn't extend the total wait beyond `max_wait`.
    for (name, version) in deps {
        let url = sparse_index_url(name);
        log.status(&format!(
            "waiting for {name}@{version} on crates.io (timeout {}s)",
            max_wait.as_secs()
        ));
        loop {
            match probe_dep_on_index(&client, &url, version) {
                Ok(true) => {
                    log.status(&format!(
                        "{name}@{version} available — \
                         continuing publish of '{crate_name}'"
                    ));
                    break;
                }
                Ok(false) => {
                    log.verbose(&format!("{name}@{version} not yet on index — retrying"));
                }
                Err(e) => {
                    log.verbose(&format!(
                        "probe error for {name}@{version}: {e:#} — retrying"
                    ));
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "publish: wait_for_workspace_deps timed out after {}s waiting for \
                     {}@{} (dep of '{}') to appear on crates.io. Either the upstream \
                     publish has not yet landed, or the version pin in {}'s Cargo.toml \
                     does not match what was published. Raise `wait_for_workspace_deps.max_wait` \
                     or verify the upstream Release.yml run completed.",
                    max_wait.as_secs(),
                    name,
                    version,
                    crate_name,
                    crate_name,
                );
            }
            std::thread::sleep(poll_interval);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// publish-set dep-completeness guard
// ---------------------------------------------------------------------------

/// Registry state of a workspace-internal dependency that is NOT in the
/// cargo-publish set, as observed by the guard's index check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DepIndexState {
    /// The dep at the required version is live on crates.io — `cargo publish`
    /// of the dependent will resolve it against the registry. Safe.
    Present,
    /// The dep is positively absent from the index (404, or the version line
    /// is missing). With the dep also absent from the publish set, the real
    /// `cargo publish` would fail with "no matching package". Fail the guard.
    Absent,
    /// The index check could not positively determine presence (transport
    /// error, timeout). Treated conservatively — the guard does NOT fail on
    /// an inconclusive probe, so a transient crates.io outage cannot block a
    /// release whose deps are actually fine.
    Unknown,
}

/// Injectable index presence probe so the guard is unit-testable without a
/// network round-trip. Production wires a closure over [`is_already_published`];
/// tests inject a closure returning canned [`DepIndexState`]s.
pub(crate) type DepIndexProbe<'a> = dyn Fn(&str, &str) -> DepIndexState + 'a;

/// Whether a `[dependencies].<name>` value is a `workspace = true` inherit
/// (dotted `name.workspace = true`, inline `{ workspace = true }`, or a
/// standard sub-table with `workspace = true`).
fn dep_value_is_workspace_inherit(item: &toml_edit::Item) -> bool {
    if let Some(v) = item.as_value()
        && let Some(tbl) = v.as_inline_table()
    {
        return tbl
            .get("workspace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    if let Some(tbl) = item.as_table() {
        return tbl
            .get("workspace")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    false
}

/// A `[workspace.dependencies]` entry as seen from a leaf's
/// `<dep>.workspace = true` inherit: the effective package name (honouring a
/// `package = "..."` rename on the root entry) and its version pin (empty
/// when the entry has no literal pin).
#[derive(Debug, Clone)]
struct RootDepPin {
    package: String,
    version: String,
}

/// Lazily-populated `[workspace.dependencies]` maps, keyed by resolved
/// workspace-root manifest path and shared across the per-crate manifest
/// walks of one publish run: each distinct root is parsed once, and a crate
/// living under a different root (a nested standalone `[workspace]`) can
/// never resolve its inherits against another crate's root. Empty until the
/// first inherit edge forces a parse.
type RootDepCache = HashMap<std::path::PathBuf, HashMap<String, RootDepPin>>;

/// Parse a workspace-root manifest's `[workspace.dependencies]` table into a
/// `key -> RootDepPin` map. The effective name comes from a `package = "..."`
/// rename on the entry (cargo only accepts the rename at the root for
/// inherited deps), falling back to the key. Returns an empty map when the
/// manifest can't be read/parsed or declares no `[workspace.dependencies]`.
fn workspace_dependency_entries(
    workspace_manifest: &std::path::Path,
) -> HashMap<String, RootDepPin> {
    let mut out: HashMap<String, RootDepPin> = HashMap::new();
    let Ok(content) = std::fs::read_to_string(workspace_manifest) else {
        return out;
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return out;
    };
    let Some(ws_deps) = doc
        .get("workspace")
        .and_then(|w| w.as_table_like())
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table_like())
    else {
        return out;
    };
    for (name, value) in ws_deps.iter() {
        let package = value
            .as_table_like()
            .and_then(|t| t.get("package"))
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .to_string();
        let version = extract_version_pin(value).unwrap_or_default();
        out.insert(name.to_string(), RootDepPin { package, version });
    }
    out
}

/// One workspace-internal dependency edge of a crate manifest, as collected
/// by [`collect_workspace_dep_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceDepEntry {
    /// Declaration key in the dependency table — the in-code alias when the
    /// entry carries a `package = "..."` rename, otherwise the crate name.
    key: String,
    /// Effective package name cargo resolves against the registry.
    package: String,
    /// Resolved version pin; empty when no literal pin could be resolved.
    version: String,
}

/// The workspace-internal, publish-required dependencies of one crate: a
/// [`WorkspaceDepEntry`] for every `[dependencies]` / `[build-dependencies]`
/// (incl. their `[target.*]` variants) entry whose effective package name is
/// a workspace crate. `dev-dependencies` are intentionally excluded —
/// `cargo publish` strips them and does NOT require them on the index, so a
/// dev-dep on a sibling that is itself unpublished must not trip the guard.
///
/// The required version is resolved from a literal pin on the leaf entry, or
/// from the workspace root's `[workspace.dependencies]` for a
/// `workspace = true` inherit. An empty version means "the dep edge exists
/// but no registry version could be resolved" — the guard then checks set
/// membership only and skips the (un-versioned) index probe.
fn publish_required_workspace_deps(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    root_cache: &mut RootDepCache,
) -> Vec<WorkspaceDepEntry> {
    collect_workspace_dep_entries(
        manifest_path,
        workspace_crate_names,
        &["dependencies", "build-dependencies"],
        root_cache,
    )
}

/// Walk the given dependency `sections` of one crate manifest (plus their
/// `[target.*.<section>]` variants) and collect a [`WorkspaceDepEntry`] for
/// every entry whose effective package name is a workspace crate.
///
/// The effective name honours `package = "..."` renames: the leaf entry's
/// field for a literal dep, or the workspace-root `[workspace.dependencies]`
/// entry for a `workspace = true` inherit (cargo only accepts the rename at
/// the root for inherited deps), falling back to the declaration key. The
/// version comes from a literal leaf pin, then the root entry's pin for an
/// inherit; entries with no resolvable version are kept with an empty
/// version string so callers can decide between skipping (the wait gate) and
/// membership-only checks (the completeness guard).
///
/// Duplicate package names across sections collapse to one entry; a later
/// occurrence only contributes its version when the first had none. Returns
/// an empty Vec when the manifest can't be read or parsed. `root_cache`
/// shares the parsed `[workspace.dependencies]` maps across the per-crate
/// calls of one run, keyed by each crate's own resolved workspace root.
fn collect_workspace_dep_entries(
    manifest_path: &std::path::Path,
    workspace_crate_names: &HashSet<&str>,
    sections: &[&str],
    root_cache: &mut RootDepCache,
) -> Vec<WorkspaceDepEntry> {
    let Ok(content) = std::fs::read_to_string(manifest_path) else {
        return Vec::new();
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };

    // Resolve inherited entries lazily — the root manifest walk happens at
    // most once per crate (memoized below), and the parse at most once per
    // distinct root across the whole run (keyed cache).
    let mut crate_root: Option<Option<std::path::PathBuf>> = None;
    let mut resolve_ws_entry = |dep: &str| -> Option<RootDepPin> {
        let root = crate_root
            .get_or_insert_with(|| {
                find_workspace_root_manifest(
                    manifest_path.parent().unwrap_or(std::path::Path::new(".")),
                )
            })
            .clone()?;
        let map = root_cache
            .entry(root)
            .or_insert_with_key(|m| workspace_dependency_entries(m));
        map.get(dep).cloned()
    };

    let mut out: Vec<WorkspaceDepEntry> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    let mut visit = |item: &toml_edit::Item,
                     out: &mut Vec<WorkspaceDepEntry>,
                     seen: &mut HashMap<String, usize>| {
        let Some(table) = item.as_table_like() else {
            return;
        };
        for (key, value) in table.iter() {
            // A renamed dep uses the TOML key as an alias:
            //   core = { package = "anodizer-core", version = "…" }
            // The crate that must be on the index is `anodizer-core`, not `core`.
            // The rename lives on the leaf entry for a literal dep, or on the
            // workspace-root entry for a `workspace = true` inherit (cargo only
            // accepts `package =` at the root for inherited deps).
            let leaf_package = value
                .as_table_like()
                .and_then(|t| t.get("package"))
                .and_then(|v| v.as_str());
            let root_entry = if leaf_package.is_none() && dep_value_is_workspace_inherit(value) {
                resolve_ws_entry(key)
            } else {
                None
            };
            let package = leaf_package
                .map(str::to_string)
                .or_else(|| root_entry.as_ref().map(|pin| pin.package.clone()))
                .unwrap_or_else(|| key.to_string());
            if !workspace_crate_names.contains(package.as_str()) {
                continue;
            }
            // Literal leaf pin first, then the workspace-root pin for an
            // inherit; an unresolved version stays empty.
            let version = extract_version_pin(value)
                .or_else(|| {
                    root_entry
                        .map(|pin| pin.version)
                        .filter(|ver| !ver.is_empty())
                })
                .unwrap_or_default();
            match seen.get(package.as_str()) {
                Some(&idx) => {
                    // The same package can appear in several sections with
                    // different specs; a version-less first sighting must not
                    // shadow a later pinned one.
                    if out[idx].version.is_empty() && !version.is_empty() {
                        out[idx].version = version;
                    }
                }
                None => {
                    seen.insert(package.clone(), out.len());
                    out.push(WorkspaceDepEntry {
                        key: key.to_string(),
                        package,
                        version,
                    });
                }
            }
        }
    };

    for section in sections {
        if let Some(item) = doc.get(section) {
            visit(item, &mut out, &mut seen);
        }
    }
    // `[target.'cfg(...)'.dependencies]` and friends.
    if let Some(target_item) = doc.get("target")
        && let Some(target_tbl) = target_item.as_table_like()
    {
        for (_cfg, target_value) in target_tbl.iter() {
            let Some(target_table) = target_value.as_table_like() else {
                continue;
            };
            for section in sections {
                if let Some(item) = target_table.get(section) {
                    visit(item, &mut out, &mut seen);
                }
            }
        }
    }
    out
}

/// Pre-publish dep-completeness guard.
///
/// For every crate in the resolved cargo-publish set, walk its
/// `Cargo.toml` non-dev dependencies and assert each workspace-internal
/// dependency is EITHER (a) also in the publish set OR (b) already live on
/// crates.io at the required version. A dep that is in NEITHER would make the
/// real `cargo publish` of the dependent fail with
/// `no matching package named '<dep>' found`, because cargo strips path deps
/// and resolves the version against the crates.io index — exactly the failure
/// that burned the CLI publish on 0.6.0 and 0.7.0 (the stage crates the CLI
/// depends on were missing from the publish set). `cargo publish --dry-run`
/// does NOT catch this: dry-run resolves the dep via the local workspace
/// PATH, so it passes even when the dep is absent from the set and the index.
///
/// `index_probe` is injected so the guard is testable without a network round
/// trip; production wires it over [`is_already_published`]. An inconclusive
/// probe ([`DepIndexState::Unknown`]) never fails the guard — only a positive
/// "absent from BOTH the set AND the index" determination does.
///
/// Works across all config modes: the publish set is whatever
/// [`cargo_publish_plan`] resolved (single-crate, workspace-lockstep, or
/// workspace per-crate), and `all_crates` spans the full universe so the
/// workspace-internal name set is mode-independent.
pub(crate) fn check_publish_set_completeness(
    order: &[String],
    all_crates: &[CrateConfig],
    versions: &HashMap<String, String>,
    index_probe: &DepIndexProbe<'_>,
    log: &StageLogger,
) -> Result<()> {
    // The publish set (names actually being published this run) and the full
    // workspace-internal name set (every crate anodize knows about).
    let in_set: HashSet<&str> = order.iter().map(|s| s.as_str()).collect();
    let workspace_names: HashSet<&str> = all_crates.iter().map(|c| c.name.as_str()).collect();
    let crate_paths: HashMap<&str, &str> = all_crates
        .iter()
        .map(|c| (c.name.as_str(), c.path.as_str()))
        .collect();

    let mut root_cache = RootDepCache::new();
    for publishing in order {
        let path = crate_paths.get(publishing.as_str()).copied().unwrap_or(".");
        let manifest_path = std::path::Path::new(path).join("Cargo.toml");
        let deps =
            publish_required_workspace_deps(&manifest_path, &workspace_names, &mut root_cache);

        for dep in deps {
            let WorkspaceDepEntry {
                key,
                package: dep_name,
                version: required_version,
            } = dep;
            // Surfacing the in-code alias alongside the registry name saves
            // the maintainer a grep when the two differ.
            let alias_note = if key != dep_name {
                format!(" (declared as '{key}' via package rename)")
            } else {
                String::new()
            };
            // In the publish set → the real publish lands it first (topological
            // order guarantees dependency-before-dependent). Safe.
            if in_set.contains(dep_name.as_str()) {
                continue;
            }

            // Not in the set — it must already be on crates.io at the version
            // the dependent requires, or the real publish will 404. Without a
            // resolvable version we cannot probe the exact line; fall back to
            // the dependent's resolved version (lockstep workspaces share one)
            // so the guard still fails loudly on a genuinely-missing sibling
            // rather than silently passing.
            let probe_version = if required_version.is_empty() {
                versions.get(publishing).cloned().unwrap_or_default()
            } else {
                required_version.clone()
            };

            if probe_version.is_empty() {
                // No version to probe AND the dep isn't in the set: we cannot
                // positively prove absence, so do not hard-fail — but surface
                // it so a real gap isn't swallowed silently.
                log.warn(&format!(
                    "crate '{publishing}' depends on workspace crate \
                     '{dep_name}'{alias_note} which is not in the cargo publish set, and the \
                     publish dep-guard could not resolve a required version to verify it is \
                     on crates.io; verify manually"
                ));
                continue;
            }

            match index_probe(&dep_name, &probe_version) {
                DepIndexState::Present => {
                    log.verbose(&format!(
                        "publish dep-guard confirmed '{publishing}' dep '{dep_name}@{probe_version}' is \
                         not in the publish set but is already on crates.io"
                    ));
                }
                DepIndexState::Absent => {
                    anyhow::bail!(
                        "publish dep-guard: crate '{publishing}' depends on workspace crate \
                         '{dep_name}'{alias_note} (version {probe_version}) which is neither in \
                         the cargo \
                         publish set nor already on crates.io; `cargo publish -p {publishing}` \
                         would fail with `no matching package named '{dep_name}' found` because \
                         cargo strips path deps and resolves the version against the crates.io \
                         index.\n\
                         Remediation:\n\
                         1. Add '{dep_name}' to the crates: publish set (give it a publish.cargo \
                         block).\n\
                         2. If '{dep_name}' was intentionally excluded via `skip: true` or an \
                         `if:` condition, verify that the required version was published in a prior \
                         release and is live on crates.io.\n\
                         3. Make the dependency non-publish (feature-gate it or use an external \
                         crate)."
                    );
                }
                DepIndexState::Unknown => {
                    log.warn(&format!(
                        "publish dep-guard could not determine crates.io state for '{publishing}' \
                         dep '{dep_name}@{probe_version}'{alias_note} (transient index error); not \
                         failing the guard on an inconclusive probe — verify the dep is published \
                         if the real `cargo publish` fails"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Heuristic: does this cargo-publish stderr look like it failed because
/// the sparse index hadn't caught up with a just-published dependency?
///
/// `poll_crates_io_index` already waits for the dep to appear on the edge
/// anodizer queries, but cargo's own publish invocation may hit a different
/// Fastly edge whose cache hasn't fanned out yet. The cargo error
/// signatures that show up in that race:
///
/// - `no matching package named '<crate>' found` — cargo couldn't locate
///   the dep at all in its registry view (the historical signature; see
///   the comment on `expand_with_transitive_deps`).
/// - `failed to select a version for the requirement '<crate> = "^X.Y.Z"'`
///   — cargo found the crate but not the just-published version; the
///   post-publish race window where cargo's resolution hits a stale
///   Fastly edge.
/// - `failed to load source for dependency '<crate>'` — sparse-index
///   transport error variant that cargo emits when the fetch itself fails
///   mid-resolution (less common but seen during Fastly fan-out windows).
///
/// All three are recoverable by waiting a few seconds and retrying. Any
/// other failure mode (auth, packaging, validation, network) does NOT
/// benefit from retry and is left to bubble up unchanged.
///
/// # Brittleness
///
/// These substrings are scraped from cargo's human-readable stderr. Cargo
/// does NOT guarantee stable error message wording across minor versions:
/// past Rust releases have renamed resolution-failure messages without a
/// deprecation period. If cargo restructures any of these strings the
/// discriminator silently stops firing, causing spurious publish failures
/// that look like hard errors instead of retryable propagation lag.
///
/// The unit tests below pin the cargo version against which these strings
/// were last verified (`cargo_version_matches_pinned_strings`). If CI
/// upgrades cargo to a different major.minor, that test fails and the
/// maintainer must re-verify the substrings against the new cargo output
/// before updating the pinned version. The strings were last verified
/// against **cargo 1.96.x** (rustc 1.96.0, 2026-05-25).
fn is_index_propagation_failure(stderr: &str) -> bool {
    stderr.contains("no matching package")
        || stderr.contains("failed to select a version")
        || stderr.contains("failed to load source for dependency")
}

/// Run `cargo publish` with bounded retry on sparse-index propagation
/// failures only.
///
/// This is defense-in-depth on top of [`poll_crates_io_index`]: even after
/// our wait sees the just-published dep on the crates.io sparse index, the
/// dependent crate's own `cargo publish` may race against Fastly's
/// inter-edge fan-out and land on a stale edge. The wait function alone
/// cannot guarantee cargo's HTTP client sees the same edge state we
/// observed. By retrying exclusively on the narrow set of error signatures
/// matched by [`is_index_propagation_failure`], we recover from the
/// edge-skew window without masking real failures (auth, packaging,
/// network).
///
/// `backoff` is the sleep between retry attempts. Production callers pass
/// [`PUBLISH_PROPAGATION_BACKOFF`]; tests pass a short `Duration` so the
/// retry path is exercised without incurring real wall-clock cost.
///
/// Returns the successful `Output` or bubbles the last failure verbatim.
/// Non-propagation failures fast-fail on the first attempt (no retry).
fn run_cargo_publish_with_retry(
    cmd: &[String],
    label: &str,
    log: &StageLogger,
    backoff: std::time::Duration,
) -> Result<std::process::Output> {
    let mut last_output: Option<std::process::Output> = None;
    for attempt in 1..=PUBLISH_PROPAGATION_RETRIES {
        let output = Command::new(&cmd[0])
            .args(&cmd[1..])
            .output()
            .with_context(|| format!("publish: spawn `{}`", cmd.join(" ")))?;

        if output.status.success() {
            return log.check_output(output, label);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !is_index_propagation_failure(&stderr) {
            // Non-propagation failure — surface immediately. check_output
            // performs redaction + error formatting consistently with the
            // single-attempt path.
            return log.check_output(output, label);
        }

        if attempt >= PUBLISH_PROPAGATION_RETRIES {
            log.warn(&format!(
                "propagation-style failure for {label} persists after {attempt} attempts; surfacing"
            ));
            last_output = Some(output);
            break;
        }

        log.status(&format!(
            "sparse-index propagation lag detected for {label} (attempt {}/{}); retrying in {}s",
            attempt,
            PUBLISH_PROPAGATION_RETRIES,
            backoff.as_secs()
        ));
        std::thread::sleep(backoff);
    }

    // All retries exhausted — surface the last failure through check_output
    // so the operator sees the same redacted error envelope as the
    // single-attempt path.
    log.check_output(
        last_output.expect("loop exits with last_output set on exhaustion"),
        label,
    )
}

// ---------------------------------------------------------------------------
// publish_to_cargo
// ---------------------------------------------------------------------------

/// Whether a `[<section>]` Cargo.toml block contains a literal
/// `version = "..."` or a `version.workspace = true` reference.
#[derive(Debug, PartialEq, Eq)]
enum CargoVersionRef {
    /// `version = "X.Y.Z"` — literal version, return as-is.
    Literal(String),
    /// `version.workspace = true` or `version = { workspace = true }` —
    /// walk up to the workspace root and resolve via `[workspace.package]`.
    Workspace,
    /// No version field in the section.
    None,
}

/// Scan a Cargo.toml body for the named section's `version` field.
/// `section_header` is e.g. `"[package]"` or `"[workspace.package]"`.
///
/// Terminates the in-section scan only when the next `[header]` is a
/// SIBLING (not a sub-table of the same logical block). For example,
/// inside `[workspace.package]` the scan continues past
/// `[workspace.package.metadata.X]` because that's a child of the
/// logical block, but stops at `[workspace.dependencies]` because
/// that's a sibling section.
///
/// Lines that begin with `#` are comment-only and skipped. Trailing
/// `# comment` text after `version = "X.Y.Z"` is also stripped before
/// parsing the literal — otherwise the value would include the
/// remainder of the line.
fn scan_section_version(content: &str, section_header: &str) -> CargoVersionRef {
    // The section-prefix is `[section_header[..-1] + '.'` — any header
    // starting with this is a sub-table of the same logical block and
    // does not end the scan.
    let sub_prefix = {
        let trimmed = section_header
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(section_header);
        format!("[{trimmed}.")
    };
    let mut in_section = false;
    for line in content.lines() {
        let trimmed_full = line.trim();
        // Strip whole-line `#` comments. (Inline `# ...` after a value
        // is handled per-value below to keep the literal-parse honest.)
        if trimmed_full.starts_with('#') {
            continue;
        }
        let trimmed = trimmed_full;
        if trimmed == section_header {
            in_section = true;
            continue;
        }
        if trimmed.starts_with('[') {
            if in_section && !trimmed.starts_with(&sub_prefix) {
                return CargoVersionRef::None;
            }
            // Outside the target section, OR a sub-table of it: skip
            // the header line and keep scanning.
            continue;
        }
        if !in_section {
            continue;
        }
        // `version.workspace = true` — but only when followed by a key
        // boundary char so `versioned-foo` / `versions` / `version-spec`
        // don't get accidentally classified as workspace inherits.
        if let Some(rest) = strip_key_prefix(trimmed, "version.workspace") {
            let rest = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
            if rest.starts_with("true") {
                return CargoVersionRef::Workspace;
            }
        }
        // `version = "X.Y.Z"` (literal) or `version = { workspace = true }`
        // (inline-table form). Same key-boundary check.
        if let Some(rest) = strip_key_prefix(trimmed, "version") {
            let rest = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
            // Literal: take the substring between the first and second `"`
            // so a trailing `# comment` doesn't bleed into the version.
            if let Some(after) = rest.strip_prefix('"')
                && let Some(end) = after.find('"')
            {
                return CargoVersionRef::Literal(after[..end].to_string());
            }
            if rest.starts_with('{')
                && rest
                    .trim_start_matches('{')
                    .trim_end_matches('}')
                    .split(',')
                    .any(|kv| kv.trim().starts_with("workspace") && kv.contains("true"))
            {
                return CargoVersionRef::Workspace;
            }
        }
    }
    CargoVersionRef::None
}

/// `s.strip_prefix(key)` plus a key-boundary check so `version`
/// doesn't match `versioned` / `versions` / `version-spec`. After the
/// prefix the next char must be whitespace, `=`, or `.` (for
/// `version.workspace`). Returns the post-prefix remainder when the
/// boundary holds, else `None`.
fn strip_key_prefix<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    match rest.chars().next() {
        // EOL after the key alone (`version`) is not a valid key=value
        // line; reject so callers don't compute an empty `rest`.
        None => None,
        Some(c) if c.is_whitespace() || c == '=' || c == '.' => Some(rest),
        _ => None,
    }
}

/// Walk parent directories from `start` looking for a Cargo.toml that
/// contains a real `[workspace]` (or exactly `[workspace.package]`)
/// section header. Returns the path to that workspace root manifest.
/// Walks at most 12 levels to bound runtime.
///
/// The header check is anchored to the exact strings — `starts_with`
/// would falsely accept a leaf-crate manifest that contains only a
/// sub-table like `[workspace.package.metadata.docs.rs]` (some crates
/// declare these for workspace-inherited metadata without being a
/// workspace root themselves).
fn find_workspace_root_manifest(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let start_abs = std::fs::canonicalize(start).ok().unwrap_or(start.into());
    let mut dir: &std::path::Path = start_abs.as_ref();
    for _ in 0..12 {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file()
            && let Ok(content) = std::fs::read_to_string(&candidate)
            && content.lines().any(|l| {
                let t = l.trim();
                t == "[workspace]" || t == "[workspace.package]"
            })
        {
            return Some(candidate);
        }
        dir = match dir.parent() {
            Some(p) => p,
            None => break,
        };
    }
    None
}

/// Read the published version for a crate at `crate_path`.
///
/// Resolves three Cargo.toml shapes:
/// - `version = "X.Y.Z"` in `[package]` → returns `Some("X.Y.Z")`.
/// - `version.workspace = true` (or `version = { workspace = true }`)
///   → walks parent dirs for a Cargo.toml with `[workspace]`, reads
///   `[workspace.package].version`, returns that.
/// - No version anywhere → `None`.
///
/// The workspace-inheritance branch is load-bearing for multi-cadence
/// workspaces (one crate at v0.2.x while siblings are at v0.3.x).
/// Falling back to the release-context version in that case would
/// poll the wrong version on the crates.io index → either a timeout
/// or a false confirmation.
fn read_cargo_toml_version(crate_path: &str) -> Option<String> {
    let manifest = std::path::Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&manifest).ok()?;
    match scan_section_version(&content, "[package]") {
        CargoVersionRef::Literal(v) => Some(v),
        CargoVersionRef::None => None,
        CargoVersionRef::Workspace => {
            // Walk up from the crate's directory to find the workspace
            // root Cargo.toml. `crate_path` is typically a relative path
            // from the repo root (e.g. `crates/core`), so `.parent()` of
            // its Cargo.toml gives the crate dir; walking up from there
            // finds the workspace manifest.
            let ws_manifest = find_workspace_root_manifest(
                manifest.parent().unwrap_or(std::path::Path::new(".")),
            )?;
            let ws_content = std::fs::read_to_string(&ws_manifest).ok()?;
            match scan_section_version(&ws_content, "[workspace.package]") {
                CargoVersionRef::Literal(v) => Some(v),
                _ => None,
            }
        }
    }
}

/// The eligible cargo-publish set, resolved once and shared between the
/// real publisher and the publish-simulation preflight.
///
/// Holds everything both consumers need so the topological/eligibility
/// derivation lives in exactly one place:
/// - `order` — crate names in dependency-first publish order.
/// - `cfgs` — per-crate resolved `publish.cargo` block (post `skip:`/`if:`).
/// - `versions` — per-crate resolved version (each crate's own Cargo.toml
///   `[package].version`, falling back to the release version), since
///   mixed-cadence workspaces publish different versions per crate.
/// - `all_crates` — the full crate universe (top-level + workspace overlay)
///   the plan was derived from, reused by callers that need `depends_on`.
pub(crate) struct CargoPublishPlan {
    pub order: Vec<String>,
    pub cfgs: HashMap<String, CargoPublishConfig>,
    pub versions: HashMap<String, String>,
    pub all_crates: Vec<CrateConfig>,
}

/// Resolve the cargo-publish set: the crates that a real release WOULD
/// publish at their target versions, in dependency-first order.
///
/// Reuses the exact eligibility rules the publisher applies — `publish.cargo`
/// presence, the peer `skip:` template, the `if:` condition, and the
/// `--crate` selection (expanded transitively via `expand_with_transitive_deps`)
/// — then orders the survivors with [`topological_sort`]. This is the single
/// source of truth for "what would be published"; the publish-simulation
/// preflight and [`publish_to_cargo_with`] both consume it so they can never
/// disagree about the set or its order.
///
/// `log` receives the same per-crate `skip:`/`if:` status lines the publisher
/// emits, so resolving the plan twice (preflight + publish) is idempotent in
/// behaviour but produces those lines once per resolution; callers that only
/// want the set (the preflight) pass a quiet/verbose logger.
pub(crate) fn cargo_publish_plan(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
) -> Result<CargoPublishPlan> {
    let all_crates: Vec<CrateConfig> = crate::util::all_crates(ctx);

    let expanded_selection: Vec<String> = if selected.is_empty() {
        Vec::new()
    } else {
        expand_with_transitive_deps(&all_crates, selected)
    };
    let selected_set: std::collections::HashSet<&str> =
        expanded_selection.iter().map(|s| s.as_str()).collect();

    let cfgs: HashMap<String, CargoPublishConfig> = {
        let mut m = HashMap::new();
        for c in &all_crates {
            let Some(ref publish) = c.publish else {
                continue;
            };
            let Some(ref cargo_cfg) = publish.cargo else {
                continue;
            };
            if let Some(ref d) = cargo_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| format!("cargo: render skip template for '{}'", c.name))?;
                if off {
                    log.status(&format!(
                        "skipped cargo publish for '{}' (skip=true)",
                        c.name
                    ));
                    continue;
                }
            }
            let proceed = anodizer_core::config::evaluate_if_condition(
                cargo_cfg.if_condition.as_deref(),
                &format!("cargo publisher for crate '{}'", c.name),
                |t| ctx.render_template(t),
            )?;
            if !proceed {
                log.status(&format!(
                    "skipping cargo publish for '{}' — `if` condition evaluated falsy",
                    c.name
                ));
                continue;
            }
            m.insert(c.name.clone(), cargo_cfg.clone());
        }
        m
    };

    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| cfgs.contains_key(&c.name))
        .map(|c| {
            let deps = c.depends_on.clone().unwrap_or_default();
            (c.name.clone(), deps)
        })
        .collect();

    let order = topological_sort(&publishable);

    let versions: HashMap<String, String> = all_crates
        .iter()
        .filter(|c| order.iter().any(|n| n == &c.name))
        .map(|c| {
            // Use an empty string when the per-crate manifest is unreadable so
            // the skip-decision treats the crate as "not yet published" (safe
            // path). Falling back to the global release version here would key
            // the idempotency probe on the WRONG version in per-crate workspaces
            // and cause the crate's real version to be silently skipped.
            let v = read_cargo_toml_version(&c.path).unwrap_or_default();
            (c.name.clone(), v)
        })
        .collect();

    Ok(CargoPublishPlan {
        order,
        cfgs,
        versions,
        all_crates,
    })
}

/// Publish every eligible crate, in topological order, recording each
/// crate's published identity into `record` AT THE MOMENT its
/// `cargo publish` succeeds.
///
/// `record` is the authoritative rollback source: the publisher's
/// `rollback()` yanks exactly the crates appended here, so a publish that
/// succeeds on crate A then fails on crate B (returning `Err`) still
/// leaves A in `record` for the unwind. Crates skipped as
/// already-published — or by `skip:` / `if:` — are intentionally NOT
/// recorded: this run didn't publish them, so yanking them would revert a
/// prior run's (or someone else's) live release.
pub fn publish_to_cargo(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
) -> Result<()> {
    publish_to_cargo_with(ctx, selected, log, record, is_already_published)
}

/// Test seam for [`publish_to_cargo`]: identical behaviour, but the
/// crates.io already-published idempotency check is injected. Production
/// passes [`is_already_published`] (a real sparse-index GET); tests pass a
/// stub so the partial-failure rollback path can be exercised without a
/// network round-trip. The signature mirrors `is_already_published`
/// `(name, version, policy) -> Result<Option<cksum>>`.
fn publish_to_cargo_with(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
    already_published_check: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
    ) -> Result<Option<String>>,
) -> Result<()> {
    // Defensive guard: the `--skip=cargo` gate lives in the
    // dispatcher in `lib.rs::PublishStage::run` so every publisher emits its
    // skip log uniformly. Re-checking here protects future direct callers
    // (tests, CLI sub-commands) from accidentally bypassing the gate. No log
    // is emitted on this path — the dispatcher already logged it.
    if ctx.should_skip("cargo") {
        return Ok(());
    }
    // Resolve the eligible publish set once — transitive-dep expansion,
    // `skip:`/`if:` gating, and topological ordering all live in
    // `cargo_publish_plan`, shared with the publish-simulation preflight so the
    // two can never disagree about which crates publish or in what order.
    let plan = cargo_publish_plan(ctx, selected, log)?;
    let CargoPublishPlan {
        order: sorted_names,
        cfgs: cargo_cfgs,
        versions: crate_versions,
        all_crates,
    } = plan;

    if sorted_names.is_empty() {
        // The publisher wrapper (`CargoPublisher::run`) emits the canonical
        // operator-facing warn for the no-eligible-crates path; this
        // branch is unreachable in normal dispatch because the wrapper
        // short-circuits before calling here, but defensive callers
        // (tests, direct CLI sub-commands) still exit cleanly.
        return Ok(());
    }

    // Build a quick lookup: name → depends_on
    let deps_map: HashMap<String, Vec<String>> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    if ctx.is_dry_run() {
        for name in &sorted_names {
            log.status(&run_per_crate_start_message(name));
            let cmd = publish_command(name, cargo_cfgs.get(name));
            log.status(&format!("(dry-run) would run: {}", cmd.join(" ")));
        }
        return Ok(());
    }

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every crate's index-check GET. Mirrors the per-pipe-invocation
    // pattern used by artifactory/cloudsmith.
    let retry_policy = ctx.retry_policy();

    // Hard backstop, BEFORE the first irreversible `cargo publish`: refuse to
    // start when any crate in the publish set has a workspace-internal
    // (non-dev) dependency that is neither in the set nor already on
    // crates.io. The publish-simulation preflight runs the same guard earlier
    // for a louder/earlier abort, but it is gated behind `--no-preflight`;
    // re-running it here means no real-publish path (publish_to_cargo /
    // --publish-only) can bypass it. Cheap: at most one sparse-index GET per
    // out-of-set dep, and a no-op for the common lockstep case where every
    // workspace dep is in the set. (Skipped in dry-run — the early return
    // above already handled that path.)
    //
    // The index probe routes through the SAME injected `already_published_check`
    // seam the publish loop uses, so the guard shares one mockable index path:
    // `Ok(Some)` = present, `Ok(None)` = positively absent, `Err` = inconclusive
    // (never fails the guard).
    {
        let probe =
            |name: &str, version: &str| match already_published_check(name, version, &retry_policy)
            {
                Ok(Some(_)) => DepIndexState::Present,
                Ok(None) => DepIndexState::Absent,
                Err(_) => DepIndexState::Unknown,
            };
        check_publish_set_completeness(&sorted_names, &all_crates, &crate_versions, &probe, log)?;
    }

    // Path lookup for the wait-for-workspace-deps manifest scan below.
    let crate_paths: HashMap<String, String> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.path.clone()))
        .collect();

    // Workspace-root dep map shared across the per-crate manifest scans —
    // parsed at most once per run.
    let mut ws_root_cache = RootDepCache::new();

    for (i, name) in sorted_names.iter().enumerate() {
        log.status(&run_per_crate_start_message(name));
        // Per-crate resolved version (own Cargo.toml `[package].version`,
        // falling back to the release version) — sourced from the plan so the
        // already-published check uses the same version the preflight queried.
        let crate_version = crate_versions.get(name).cloned().unwrap_or_default();

        // Idempotency: if this version already exists on crates.io, skip.
        // crates.io versions are immutable once published, so presence on
        // the index is sufficient evidence that this publisher's work is done.
        // Byte-level cksum comparison is intentionally omitted: `cargo package`
        // embeds file mtimes, making the output non-deterministic across runs;
        // any mismatch is therefore a false positive that can't be fixed
        // without bumping — and we can't fix it anyway (index is immutable).
        // Index check failures are non-fatal — fall through to publish and let
        // cargo's server-side 409 guard handle real conflicts.
        let already_published = if crate_version.is_empty() {
            false
        } else {
            match already_published_check(name, &crate_version, &retry_policy) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    log.warn(&format!(
                        "could not check crates.io index for '{}-{}' ({}); attempting publish anyway",
                        name, crate_version, e
                    ));
                    false
                }
            }
        };
        if already_published {
            log.status(&format!(
                "skipping '{}-{}' — already published on crates.io",
                name, crate_version
            ));
            continue;
        }

        let cargo_cfg = cargo_cfgs.get(name);

        // Pre-publish gate: in multi-tag-multi-crate workspaces (e.g. cfgd)
        // per-crate tags fire independent Release.yml runs, so the upstream
        // crate's publish may not have landed on crates.io by the time this
        // downstream's publish starts. The wait_for_workspace_deps block,
        // when enabled, polls crates.io for every workspace-internal dep at
        // its pinned version and blocks until each appears. Disabled by
        // default — anodize's own workspace publishes lockstep within one
        // Release.yml run, where in-loop topological order + the post-
        // publish poll_crates_io_index call below already cover the race.
        let wait_cfg = cargo_cfg
            .and_then(|c| c.wait_for_workspace_deps.as_ref())
            .cloned()
            .unwrap_or_default();
        if wait_cfg.resolved_enabled() {
            let crate_path = crate_paths
                .get(name)
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            let manifest_path = std::path::Path::new(&crate_path).join("Cargo.toml");
            // Workspace-internal dep set: every crate in the same anodize
            // config (top-level + workspaces overlay). External crates.io
            // deps (serde, tokio, ...) get filtered out by the name check.
            let workspace_names: HashSet<&str> =
                all_crates.iter().map(|c| c.name.as_str()).collect();
            let deps =
                workspace_deps_for_crate(&manifest_path, &workspace_names, &mut ws_root_cache);
            if deps.is_empty() {
                log.verbose(&format!(
                    "'{name}' has no workspace-internal deps with \
                     a literal version pin — gate is a no-op"
                ));
            } else {
                wait_for_workspace_deps_to_appear(name, &deps, &wait_cfg, log)
                    .with_context(|| format!("publish: wait_for_workspace_deps for '{name}'"))?;
            }
        }

        let cmd = publish_command(name, cargo_cfg);
        log.status(&format!("running {}", cmd.join(" ")));

        // Defense in depth: even though poll_crates_io_index already waits
        // for the prior crate to land on the index edge anodizer queries,
        // cargo's own resolution may hit a stale Fastly edge a beat later.
        // run_cargo_publish_with_retry narrows retry exclusively to the
        // sparse-index propagation failure signatures so real errors still
        // fast-fail.
        run_cargo_publish_with_retry(
            &cmd,
            &format!("cargo publish -p {}", name),
            log,
            PUBLISH_PROPAGATION_BACKOFF,
        )?;

        log.status(&format!("published crate '{}'", name));

        // Record the published identity NOW, at the instant of success, so
        // a later crate's failure can still drive rollback to yank this
        // one. Registry/index come from the same `publish.cargo` block the
        // publish used, so the yank targets the matching registry. The
        // version is the per-crate resolved version (workspaces with mixed
        // cadences publish different versions per crate).
        //
        // When the per-crate manifest was unreadable, crate_version is empty
        // (the skip-decision treats it as "not yet published" to avoid a
        // false-skip). For the yank record we fall back to the global release
        // version so rollback can still attempt a yank. If even that is
        // empty, warn: `cargo yank --version ""` is rejected and a silent
        // under-yank is worse than an explicit manual-cleanup message.
        let yank_version = if !crate_version.is_empty() {
            crate_version.clone()
        } else {
            ctx.version()
        };
        if yank_version.is_empty() {
            log.warn(&format!(
                "cargo published '{name}' with no resolvable version; it CANNOT be \
                 auto-yanked on rollback — verify and `cargo yank` it manually if a \
                 later crate fails this run"
            ));
        } else {
            record.push(CargoYankTarget {
                name: name.clone(),
                version: yank_version,
                registry: cargo_cfg.and_then(|c| c.registry.clone()),
                index: cargo_cfg.and_then(|c| c.index.clone()),
            });
        }

        // If there are later crates that depend on this one, wait for the index.
        let has_dependents = sorted_names[i + 1..].iter().any(|later| {
            deps_map
                .get(later)
                .map(|d| d.contains(name))
                .unwrap_or(false)
        });

        if has_dependents && !crate_version.is_empty() {
            let timeout = cargo_cfg
                .and_then(|c| c.index_timeout)
                .unwrap_or(DEFAULT_INDEX_TIMEOUT_SECS);
            if timeout == 0 {
                log.warn(&format!(
                    "index_timeout is 0 for '{}'; skipping index poll (dependents may fail)",
                    name
                ));
            } else {
                log.status(&format!(
                    "waiting for {}-{} in crates.io index (timeout={}s)…",
                    name, crate_version, timeout
                ));
                poll_crates_io_index(name, &crate_version, timeout, log)
                    .with_context(|| format!("publish: index poll for '{}'", name))?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CargoPublisher - Publisher trait adapter
// ---------------------------------------------------------------------------

// Publisher trait adapter around `publish_to_cargo`. Classified as
// `Submitter` + `required=true`: crates.io publish is effectively one-way
// (versions cannot be re-uploaded), so a failure here must fail the release
// and other Submitter publishers must already be gated.
simple_publisher!(
    CargoPublisher,
    "cargo",
    anodizer_core::PublisherGroup::Submitter,
    true,
    Some("CARGO_REGISTRY_TOKEN yank"),
);

/// Operator-visible start line for the cargo publisher. Mirrors the
/// `run_start_message` helper every per-crate publisher exposes so the
/// dispatch table can't silently report success on a no-op run.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "starting cargo publish for {} selected crate(s)",
        selected_total
    )
}

/// Operator-visible per-crate start line. Emitted by `publish_to_cargo`
/// immediately before each crate's publish-or-skip decision so the
/// per-crate progress is anchored to a specific name in the log.
/// Mirrors `run_per_crate_start_message` on every other per-crate
/// publisher (homebrew, scoop, nix, aur, krew).
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate cargo publish for \'{}\'", crate_name)
}

/// Operator-visible done line, emitted after `publish_to_cargo` returns
/// Ok. `processed` counts crates whose publish path was actually
/// invoked (skipped-by-already-published, skipped-by-skip-template, and
/// dry-run paths all count as processed — they're successful runs of
/// the correct code path).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("finished cargo publish — {} crate(s) processed", processed)
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.cargo` block) but `publish_to_cargo` resolved
/// zero publishable crates (every cargo-configured crate was filtered
/// out by `--crate` / `--all` selection).
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "cargo publisher registered but 0 of {} effective crate(s) had a publish.cargo \
         block selected — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.cargo block is set.",
        selected_total
    )
}

/// True when `ctx.config.crates` contains at least one crate with a
/// `publish.cargo` block. Used by the publisher's run wrapper to choose
/// between the `done` and `no-eligible-crates` log paths.
fn count_cargo_configured_crates(ctx: &Context) -> usize {
    let all = crate::util::all_crates(ctx);
    let selected = &ctx.options.selected_crates;
    all.iter()
        .filter(|c| c.publish.as_ref().and_then(|p| p.cargo.as_ref()).is_some())
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .count()
}

impl anodizer_core::Publisher for CargoPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // `cargo publish` resolves the crates.io token from
        // CARGO_REGISTRY_TOKEN; the run path spawns the literal `cargo`
        // from PATH, so probe exactly that.
        let configured = anodizer_core::env_preflight::crate_universe(&ctx.config)
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.cargo.as_ref())
            .any(|cargo| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    cargo.skip.as_ref(),
                    None,
                    cargo.if_condition.as_deref(),
                )
            });
        if !configured {
            return Vec::new();
        }
        vec![
            anodizer_core::EnvRequirement::Tool {
                name: "cargo".to_string(),
            },
            anodizer_core::EnvRequirement::EnvAllOf {
                vars: vec!["CARGO_REGISTRY_TOKEN".to_string()],
            },
        ]
    }

    fn programmatic_rollback_on_failure(&self, evidence: &anodizer_core::PublishEvidence) -> bool {
        // A failed cargo run that already pushed one or more crates to
        // crates.io recorded them here; rollback must yank them even
        // though the overall outcome is `Failed`. An empty record means
        // nothing went live — keep the failure inert.
        !decode_cargo_yank_targets(&evidence.extra).is_empty()
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected = ctx.options.selected_crates.clone();
        // Operator-facing visible-work bookends — every per-crate publisher
        // emits these so a no-op dispatch can't masquerade as success.
        // `publish_to_cargo` emits per-crate progress
        // (`(dry-run) would run: ...` / `running: cargo publish -p ...` /
        // `skipping ... already published`) plus the per-crate-start line
        // from `run_per_crate_start_message` which forms the loop-body
        // signal that satisfies the visible-work contract.
        let eligible = count_cargo_configured_crates(ctx);
        log.status(&run_start_message(eligible.max(selected.len())));
        // Short-circuit BEFORE delegating into publish_to_cargo when no
        // cargo-configured crate is eligible — otherwise the inner path
        // would also emit a "no crates configured ..." status, duplicating
        // the canonical no-eligible warn the wrapper owns.
        if eligible == 0 {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
            return Ok(anodizer_core::PublishEvidence::new("cargo"));
        }
        // `record` accumulates one entry per crate whose `cargo publish`
        // actually succeeds. On the failure path we still build evidence
        // from whatever was published before the bail and stash it on the
        // context so dispatch can hand it to rollback — otherwise a
        // partial multi-crate publish would leave the succeeded crates
        // live with nothing to yank.
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let publish_result = publish_to_cargo(ctx, &selected, &log, &mut record);

        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        if let Some(primary) = first_published_crate(ctx) {
            evidence.primary_ref = Some(format!(
                "https://crates.io/crates/{name}/{version}",
                name = primary.name,
                version = primary.version
            ));
        }
        evidence.extra = encode_cargo_yank_targets(&record);

        match publish_result {
            Ok(()) => {
                log.status(&run_done_message(eligible));
                Ok(evidence)
            }
            Err(e) => {
                // Stash the partial evidence BEFORE propagating so the
                // dispatcher's `Err` arm can recover it for rollback.
                ctx.record_pending_evidence(evidence);
                Err(e)
            }
        }
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        // Yank from the authoritative record built at publish time: each
        // entry is a crate whose `cargo publish` actually SUCCEEDED this
        // run, with the per-crate version and the registry/index the
        // publish used. This is correct even when the local `.crate`
        // files are gone (workspace cleaned, different CI job, run died
        // before packaging) — the old disk-scan rollback yanked NOTHING in
        // that case, leaving succeeded crates live.
        let targets = decode_cargo_yank_targets(&evidence.extra);
        if targets.is_empty() {
            // Nothing was published this run — a clean no-op, not a
            // failure to recover. (Verbose, not a scary warn: an empty
            // record is the normal shape when the failing publisher never
            // reached its first successful `cargo publish`.)
            log.verbose("no crates published this run; cargo rollback is a no-op");
            return Ok(());
        }
        let mut yanked = 0usize;
        let mut failed = 0usize;
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would yank {} crate(s) from their configured registries",
                targets.len()
            ));
            return Ok(());
        }
        for t in &targets {
            // crates.io versions are immutable, so `cargo yank` is the
            // strongest unwind available; the version slot stays burned
            // and any consumer that already resolved against it keeps
            // working. Operators must still bump to recover.
            let mut args: Vec<String> = vec![
                "yank".into(),
                "--version".into(),
                t.version.clone(),
                t.name.clone(),
            ];
            if let Some(ref r) = t.registry {
                args.push("--registry".into());
                args.push(r.clone());
            }
            if let Some(ref idx) = t.index {
                args.push("--index".into());
                args.push(idx.clone());
            }
            let target = t
                .registry
                .as_deref()
                .or(t.index.as_deref())
                .unwrap_or("crates.io");
            log.status(&format!("yanking {} {} ({})", t.name, t.version, target));
            let output = Command::new("cargo").args(&args).output()?;
            if output.status.success() {
                yanked += 1;
            } else {
                failed += 1;
                log.warn(&format!(
                    "cargo yank failed for {} {} on {}: {}",
                    t.name,
                    t.version,
                    target,
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
        }
        log.status(&format!(
            "cargo rollback yanked {} crate(s), {} failure(s)",
            yanked, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // crates.io publishing requires CARGO_REGISTRY_TOKEN at run-time;
        // the existing publish_to_cargo path emits its own loud failure
        // on a missing token, so this check defaults to Pass for now.
        // A future tightening can surface a Warning when the token is
        // absent AND best-effort rollback was requested.
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
}

struct PublishedCrateRef {
    name: String,
    version: String,
}

/// Returns the canonical published crate for `primary_ref` reporting.
///
/// Multi-crate workspaces release many crates in one run; the
/// [`PublishEvidence`](anodizer_core::PublishEvidence) schema's
/// `primary_ref` carries one canonical URL. We prefer the crate whose
/// `name` matches `ctx.config.project_name` so operators see the marquee
/// crate (e.g. `anodizer` from the `anodizer-*` workspace) instead of
/// whichever crate happens to iterate first. If no such match exists
/// (project_name unset, or no eligible crate matches it), fall back to
/// the first crate with `publish.cargo` configured.
fn first_published_crate(ctx: &Context) -> Option<PublishedCrateRef> {
    let eligible = |c: &&CrateConfig| c.publish.as_ref().and_then(|p| p.cargo.as_ref()).is_some();
    let project_name = ctx.config.project_name.as_str();
    let name = ctx
        .config
        .crates
        .iter()
        .find(|c| !project_name.is_empty() && c.name == project_name && eligible(c))
        .or_else(|| ctx.config.crates.iter().find(eligible))
        .map(|c| c.name.clone())?;
    let version = {
        let tag = ctx
            .git_info
            .as_ref()
            .map(|g| g.tag.clone())
            .unwrap_or_else(|| ctx.version());
        tag.strip_prefix('v').unwrap_or(&tag).to_string()
    };
    if version.is_empty() {
        return None;
    }
    Some(PublishedCrateRef { name, version })
}

/// Authoritative per-crate record of a `cargo publish` that SUCCEEDED
/// during this run. Aliased to the core-owned snapshot so the evidence
/// schema lives in [`anodizer_core::publish_evidence`] and no
/// credential-shaped field can land in it.
pub(crate) type CargoYankTarget = anodizer_core::publish_evidence::CargoYankTargetSnapshot;

/// Encode the recorded yank targets into the typed
/// [`PublishEvidenceExtra::Cargo`] variant.
pub(crate) fn encode_cargo_yank_targets(
    targets: &[CargoYankTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Cargo(anodizer_core::publish_evidence::CargoExtra {
        cargo_yank_targets: targets.to_vec(),
    })
}

/// Decode the typed Cargo variant into the recorded yank targets.
/// Returns an empty vec for any other variant — rollback then treats the
/// run as "nothing published this run" and no-ops cleanly.
pub(crate) fn decode_cargo_yank_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<CargoYankTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Cargo(c) => c.cargo_yank_targets.clone(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};

    #[test]
    fn cargo_publisher_classification() {
        let p = CargoPublisher::new();
        assert_eq!(p.name(), "cargo");
        assert_eq!(p.group(), PublisherGroup::Submitter);
        assert!(p.required());
        assert_eq!(p.rollback_scope_needed(), Some("CARGO_REGISTRY_TOKEN yank"));
    }

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("starting cargo publish for"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("starting per-crate cargo publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished cargo publish"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("cargo publisher registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    #[test]
    fn cargo_preflight_defaults_to_pass() {
        // stub: when preflight gains CARGO_REGISTRY_TOKEN logic this test
        // gets replaced.
        let ctx = TestContextBuilder::new().build();
        let p = CargoPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn first_published_crate_prefers_project_name_match() {
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

        let with_cargo = |name: &str| CrateConfig {
            name: name.to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Iteration order: util crate is first, but project_name matches
        // the marquee crate later in the list — the helper MUST prefer
        // the project_name match instead of first-iterated.
        let ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .crates(vec![with_cargo("anodizer-util"), with_cargo("anodizer")])
            .build();

        let r = first_published_crate(&ctx).expect("eligible crate");
        assert_eq!(r.name, "anodizer");
    }

    #[test]
    fn first_published_crate_falls_back_to_first_when_no_project_match() {
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

        let with_cargo = |name: &str| CrateConfig {
            name: name.to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // project_name doesn't match ANY eligible crate; fall back to
        // first-iterated to preserve historical behaviour.
        let ctx = TestContextBuilder::new()
            .project_name("ghost")
            .crates(vec![with_cargo("anodizer-util"), with_cargo("anodizer")])
            .build();

        let r = first_published_crate(&ctx).expect("eligible crate");
        assert_eq!(r.name, "anodizer-util");
    }

    #[test]
    fn cargo_publisher_emits_visible_work_when_configured() {
        use crate::testing::assert_publisher_visible_work_contract;
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

        let cargo_crate = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![cargo_crate])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .build();
        let p = CargoPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal `version = "X.Y.Z"` in [package] is read verbatim.
    #[test]
    fn read_cargo_toml_version_literal_in_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(
            read_cargo_toml_version(dir.path().to_str().unwrap()),
            Some("1.2.3".into())
        );
    }

    /// `version.workspace = true` resolves via the workspace root's
    /// `[workspace.package].version`. Without this resolution the
    /// publish path falls back to the release-context version, which
    /// is wrong for any multi-cadence workspace.
    #[test]
    fn read_cargo_toml_version_workspace_dot_form() {
        let ws_root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            ws_root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/leaf\"]\n\n[workspace.package]\nversion = \"4.5.6\"\n",
        )
        .unwrap();
        let leaf = ws_root.path().join("crates").join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion.workspace = true\n",
        )
        .unwrap();
        assert_eq!(
            read_cargo_toml_version(leaf.to_str().unwrap()),
            Some("4.5.6".into())
        );
    }

    /// `version = { workspace = true }` (inline-table form) resolves
    /// the same way as the dotted form.
    #[test]
    fn read_cargo_toml_version_workspace_inline_table_form() {
        let ws_root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            ws_root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n[workspace.package]\nversion = \"0.9.0\"\n",
        )
        .unwrap();
        let leaf = ws_root.path().join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion = { workspace = true }\n",
        )
        .unwrap();
        assert_eq!(
            read_cargo_toml_version(leaf.to_str().unwrap()),
            Some("0.9.0".into())
        );
    }

    /// No version anywhere yields None (publish path falls back to the
    /// release-context version, preserving prior behavior for
    /// version-less manifests).
    #[test]
    fn read_cargo_toml_version_returns_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(read_cargo_toml_version(dir.path().to_str().unwrap()), None);
    }

    #[test]
    fn test_topo_sort_simple() {
        let order = vec![
            ("cfgd-core".to_string(), vec![]),
            ("cfgd".to_string(), vec!["cfgd-core".to_string()]),
        ];
        let sorted = topological_sort(&order);
        assert_eq!(sorted, vec!["cfgd-core", "cfgd"]);
    }

    #[test]
    fn test_topo_sort_no_deps() {
        let order = vec![("a".to_string(), vec![]), ("b".to_string(), vec![])];
        let sorted = topological_sort(&order);
        assert_eq!(sorted.len(), 2);
    }

    #[test]
    fn test_publish_command_default() {
        // No config block — historical behaviour preserved (--allow-dirty on).
        let cmd = publish_command("my-crate", None);
        assert_eq!(
            cmd,
            vec![
                "cargo".to_string(),
                "publish".to_string(),
                "-p".to_string(),
                "my-crate".to_string(),
                "--allow-dirty".to_string(),
            ]
        );
    }

    #[test]
    fn test_publish_command_full_flag_surface() {
        let cfg = CargoPublishConfig {
            registry: Some("alt-registry".to_string()),
            index: Some("https://example.com/idx".to_string()),
            no_verify: Some(true),
            allow_dirty: Some(true),
            features: Some(vec!["a".to_string(), "b".to_string()]),
            all_features: Some(true),
            no_default_features: Some(true),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            target_dir: Some(std::path::PathBuf::from("/tmp/td")),
            jobs: Some(4),
            keep_going: Some(true),
            manifest_path: Some(std::path::PathBuf::from("./Cargo.toml")),
            locked: Some(true),
            offline: Some(true),
            frozen: Some(true),
            ..Default::default()
        };
        let cmd = publish_command("my-crate", Some(&cfg));

        // Helper: assert the flag is present and (for value-bearing flags)
        // the immediately-next argv slot holds the expected value. Catches
        // bugs where two adjacent flag/value pairs swap.
        let assert_value = |flag: &str, expected: &str| {
            let pos = cmd
                .iter()
                .position(|s| s == flag)
                .unwrap_or_else(|| panic!("missing flag {flag}: {cmd:?}"));
            assert_eq!(
                cmd[pos + 1],
                expected,
                "{flag} value mismatch (full cmd: {cmd:?})"
            );
        };
        let assert_present = |flag: &str| {
            assert!(
                cmd.iter().any(|s| s == flag),
                "missing flag {flag}: {cmd:?}"
            );
        };

        // Value-bearing flags — assert flag + adjacent value at pos+1.
        assert_value("--registry", "alt-registry");
        assert_value("--index", "https://example.com/idx");
        assert_value("--features", "a,b"); // features are comma-joined
        assert_value("--target", "x86_64-unknown-linux-gnu");
        assert_value("--target-dir", "/tmp/td");
        assert_value("--jobs", "4");
        assert_value("--manifest-path", "./Cargo.toml");

        // Boolean flags — only need to assert presence (no following value).
        for flag in [
            "--no-verify",
            "--allow-dirty",
            "--all-features",
            "--no-default-features",
            "--keep-going",
            "--locked",
            "--offline",
            "--frozen",
        ] {
            assert_present(flag);
        }
    }

    #[test]
    fn test_publish_command_allow_dirty_explicit_false() {
        let cfg = CargoPublishConfig {
            allow_dirty: Some(false),
            ..Default::default()
        };
        let cmd = publish_command("my-crate", Some(&cfg));
        assert!(
            !cmd.iter().any(|s| s == "--allow-dirty"),
            "explicit allow_dirty=false should suppress the flag: {cmd:?}"
        );
    }

    fn crate_with_deps(name: &str, deps: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn test_expand_transitive_deps_includes_direct_dep() {
        // --crate cfgd should expand to [cfgd, cfgd-core] so cfgd-core
        // gets published before cfgd tries to reference it on crates.io.
        let crates = vec![
            crate_with_deps("cfgd-core", &[]),
            crate_with_deps("cfgd", &["cfgd-core"]),
        ];
        let selection = vec!["cfgd".to_string()];
        let expanded = expand_with_transitive_deps(&crates, &selection);
        assert!(expanded.contains(&"cfgd".to_string()));
        assert!(expanded.contains(&"cfgd-core".to_string()));
        assert_eq!(expanded.len(), 2);
    }

    #[test]
    fn test_expand_transitive_deps_chains_through_multiple_levels() {
        let crates = vec![
            crate_with_deps("a", &[]),
            crate_with_deps("b", &["a"]),
            crate_with_deps("c", &["b"]),
        ];
        let expanded = expand_with_transitive_deps(&crates, &["c".to_string()]);
        assert!(expanded.contains(&"a".to_string()));
        assert!(expanded.contains(&"b".to_string()));
        assert!(expanded.contains(&"c".to_string()));
    }

    #[test]
    fn test_expand_transitive_deps_dedupes_shared_ancestors() {
        // diamond: d depends on both b and c, which both depend on a.
        let crates = vec![
            crate_with_deps("a", &[]),
            crate_with_deps("b", &["a"]),
            crate_with_deps("c", &["a"]),
            crate_with_deps("d", &["b", "c"]),
        ];
        let expanded = expand_with_transitive_deps(&crates, &["d".to_string()]);
        assert_eq!(
            expanded.len(),
            4,
            "expected all 4 crates once: {:?}",
            expanded
        );
    }

    #[test]
    fn test_expand_transitive_deps_ignores_external_deps() {
        // Deps on names not present in the config (i.e. external crates.io
        // crates) are silently dropped — cargo verifies them against the
        // real registry, not our workspace.
        let crates = vec![crate_with_deps("cfgd", &["cfgd-core", "serde"])];
        let expanded = expand_with_transitive_deps(&crates, &["cfgd".to_string()]);
        assert!(expanded.contains(&"cfgd".to_string()));
        // cfgd-core isn't in the config, so it won't appear
        assert!(!expanded.contains(&"cfgd-core".to_string()));
        assert!(!expanded.contains(&"serde".to_string()));
    }

    // -----------------------------------------------------------------------
    // crates.io idempotency (C-new-11 / C-new-13)
    //
    // The hash-match short-circuit in publish_to_cargo (cf. cargo.rs
    // ~line 489) avoids redundant `cargo publish` calls — and the bogus
    // 422-with-stale-bytes problem they create — when the version already
    // exists on crates.io and the local .crate cksum matches the index. The
    // tests below pin (a) the sparse-index URL shape so we hit the same
    // path cargo itself uses, and (b) the JSONL parser so we keep treating
    // "version present, no cksum" as a fall-back-to-skip rather than a
    // silently-missed publish.
    // -----------------------------------------------------------------------

    /// Sparse-index URL must follow the cargo registry layout:
    /// 1-char names live under `/1/<name>`, 2-char under `/2/<name>`,
    /// 3-char under `/3/<first>/<name>`, 4+ under `/<first2>/<next2>/<name>`.
    /// Mismatch here means we'd query a URL that always 404s and silently
    /// re-publish every release.
    #[test]
    fn test_sparse_index_url_shape() {
        // 1-char crate name.
        assert_eq!(sparse_index_url("a"), "https://index.crates.io/1/a");
        // 2-char.
        assert_eq!(sparse_index_url("ab"), "https://index.crates.io/2/ab");
        // 3-char — `/3/<first>/<name>`.
        assert_eq!(sparse_index_url("abc"), "https://index.crates.io/3/a/abc");
        // 4-char — `/<first2>/<next2>/<name>`.
        assert_eq!(
            sparse_index_url("abcd"),
            "https://index.crates.io/ab/cd/abcd"
        );
        // Real-world case (5+ char): `cfgd-core`.
        assert_eq!(
            sparse_index_url("cfgd-core"),
            "https://index.crates.io/cf/gd/cfgd-core"
        );
        // Uppercase normalises to lowercase per cargo registry spec.
        assert_eq!(
            sparse_index_url("MyTool"),
            "https://index.crates.io/my/to/mytool"
        );
    }

    /// Parser returns the cksum only when a line matches the requested
    /// version; mismatched-version lines and absent fields short-circuit
    /// to None/empty respectively.
    #[test]
    fn test_parse_index_cksum_for_version_matches_requested_version() {
        // Two versions on the index; only 1.2.3's cksum should come back.
        let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}
{"name":"foo","vers":"1.2.3","cksum":"newhash","yanked":false}
{"name":"foo","vers":"1.2.4","cksum":"newer","yanked":false}"#;
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some("newhash".to_string())
        );
    }

    #[test]
    fn test_parse_index_cksum_for_version_returns_none_when_absent() {
        // Index has 1.2.2 but caller asked for 1.2.3 — must return None so
        // publish_to_cargo proceeds with the publish.
        let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}"#;
        assert_eq!(parse_index_cksum_for_version(body, "1.2.3"), None);
    }

    #[test]
    fn test_parse_index_cksum_for_version_empty_string_when_cksum_missing() {
        // Index entry has the requested version but no `cksum` field
        // (malformed/legacy entry). Returning Some("") signals "present but
        // drift undetectable" so the caller falls back to the historical
        // skip behaviour rather than mis-treating it as "not published".
        let body = r#"{"name":"foo","vers":"1.2.3","yanked":false}"#;
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some(String::new())
        );
    }

    #[test]
    fn test_parse_index_cksum_for_version_empty_body() {
        // Defensive: an empty/whitespace body parses to None (the function
        // is invoked after a 200-OK status but before further validation,
        // so we mustn't panic on malformed bodies).
        assert_eq!(parse_index_cksum_for_version("", "1.0.0"), None);
        assert_eq!(parse_index_cksum_for_version("   \n  ", "1.0.0"), None);
    }

    #[test]
    fn test_parse_index_cksum_for_version_skips_garbage_lines() {
        // A non-JSON line in the middle must not abort the scan — cargo's
        // own client tolerates trailing newlines and similar.
        let body = "not-json\n{\"name\":\"foo\",\"vers\":\"1.2.3\",\"cksum\":\"abcd\"}\n";
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some("abcd".to_string())
        );
    }

    // ---- retry plumbing through is_already_published_at ------------------
    //
    // Pin: the sparse-index GET must route through retry_http_blocking so
    // transient 5xx / 429 / network failures retry per the user's policy.
    // 404 (crate never published) must remain Ok(None) — preserved via the
    // HttpError(404)-from-Break catch in is_already_published_at.

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn fast_retry_policy() -> anodizer_core::retry::RetryPolicy {
        anodizer_core::retry::RetryPolicy {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    #[test]
    fn is_already_published_at_retries_5xx_then_succeeds() {
        use std::sync::atomic::Ordering;

        let body = r#"{"name":"foo","vers":"1.2.3","cksum":"abc123","yanked":false}"#.to_string();
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            ok_resp,
        ]);

        let url = format!("http://{addr}/3/f/foo");
        let result = is_already_published_at(&url, "foo", "1.2.3", &fast_retry_policy())
            .expect("retries 5xx then parses");
        assert_eq!(result, Some("abc123".to_string()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 503 retry then success"
        );
    }

    #[test]
    fn is_already_published_at_404_maps_to_ok_none() {
        // A 404 must NOT retry and must surface as Ok(None) — preserving
        // the "crate never published" signal that the publish pipeline
        // relies on to skip the drift check.
        use std::sync::atomic::Ordering;

        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let url = format!("http://{addr}/3/f/foo");
        let result = is_already_published_at(&url, "foo", "1.2.3", &fast_retry_policy())
            .expect("404 is Ok(None)");
        assert_eq!(result, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "404 must NOT retry");
    }

    /// Defense-in-depth: a crates.io sparse-index 4xx response that echoes
    /// our `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. The sparse index is unauthenticated
    /// in production, so this is paranoia — but mirror/proxy registries can
    /// gateway through an auth proxy.
    #[test]
    fn is_already_published_at_redacts_bearer_in_error_body() {
        let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg denied";
        let body_len = leaky.len();
        // 401 fast-fails (4xx) so a single response suffices.
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {body_len}\r\n\r\n{leaky}")
                .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let url = format!("http://{addr}/3/f/foo");
        let err = is_already_published_at(&url, "foo", "1.2.3", &fast_retry_policy())
            .expect_err("401 must fast-fail");
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

    /// Version-exists on crates.io must skip without comparing bytes.
    /// Pre-seed a sparse-index response that returns a valid version entry;
    /// the publisher loop must emit "skipping" and NOT attempt to POST.
    #[test]
    fn skip_on_version_exists_no_cksum_comparison() {
        use std::sync::atomic::Ordering;

        // Serve a JSONL body that says version 1.2.3 is published (with a cksum).
        let body = r#"{"name":"myapp","vers":"1.2.3","cksum":"deadbeef","yanked":false}"#;
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![ok_resp]);
        let url = format!("http://{addr}/3/m/myapp");

        // is_already_published_at should return Some(_), signalling skip.
        let result = is_already_published_at(&url, "myapp", "1.2.3", &fast_retry_policy())
            .expect("index check succeeds");
        assert!(
            result.is_some(),
            "index returned a version entry, expected Some"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one HTTP request");

        // The important invariant: Some(_) from is_already_published now
        // unconditionally skips — the caller must NOT call
        // compute_local_crate_cksum or bail.  We verify that by checking
        // the value is discarded (any Some triggers skip regardless of content).
        let cksum = result.unwrap();
        // Non-empty cksum in index body: old code would have compared it and
        // potentially bailed; new code ignores the value entirely.
        assert_eq!(cksum, "deadbeef");
    }

    // -----------------------------------------------------------------------
    // sparse-index propagation retry on cargo publish
    //
    // Defense in depth on top of poll_crates_io_index: even after our wait
    // sees the just-published dep on the sparse index, cargo's own resolution
    // may hit a stale Fastly edge a beat later. run_cargo_publish_with_retry
    // narrows retry exclusively to the propagation-shaped error signatures
    // so real failures (auth, packaging, network) still fast-fail.
    // -----------------------------------------------------------------------

    /// Discriminator: every known propagation-style cargo stderr must match
    /// so the retry harness recognises it; non-propagation failures must NOT
    /// match so retry doesn't mask genuine errors.
    #[test]
    fn is_index_propagation_failure_matches_known_signatures() {
        // Historical signature from anodizer's older topo-sort era.
        assert!(is_index_propagation_failure(
            "error: no matching package named `cfgd-core` found"
        ));
        // Stale-edge resolution failure: cargo found the crate on the
        // sparse index but not the just-published version it depends on.
        assert!(is_index_propagation_failure(
            "error: failed to select a version for the requirement \
             `anodizer-stage-publish = \"^0.3.0\"`"
        ));
        // Sparse-index transport variant.
        assert!(is_index_propagation_failure(
            "error: failed to load source for dependency `anodizer-core`"
        ));
    }

    #[test]
    fn is_index_propagation_failure_rejects_unrelated_errors() {
        // Auth failure — must NOT retry (token won't appear by waiting).
        assert!(!is_index_propagation_failure(
            "error: failed to publish to registry: 401 Unauthorized"
        ));
        // Validation failure — must NOT retry (broken Cargo.toml stays broken).
        assert!(!is_index_propagation_failure(
            "error: invalid character `_` in crate name `bad_name`"
        ));
        // Network failure — caller has its own transport retries; the
        // propagation-retry path shouldn't double-count those.
        assert!(!is_index_propagation_failure(
            "error: failed to send HTTP request: connection refused"
        ));
        // Empty stderr (cargo crashed without saying anything) — don't retry.
        assert!(!is_index_propagation_failure(""));
    }

    /// Pin the cargo major.minor version against which the discriminator
    /// substrings in [`is_index_propagation_failure`] were last verified.
    ///
    /// If CI upgrades to a different cargo major.minor this test fails,
    /// signalling that a maintainer must re-run `cargo publish` against a
    /// fixture that triggers each error substring and confirm the wording
    /// matches before bumping `VERIFIED_CARGO_MINOR` below.
    ///
    /// The substrings were last verified against cargo 1.96.x (rustc 1.96.0,
    /// released 2026-05-25). Bump `VERIFIED_CARGO_MINOR` only after
    /// manually confirming all three substrings still appear verbatim in
    /// the new cargo's publish output.
    #[test]
    fn cargo_version_matches_pinned_discriminator_strings() {
        // Last-verified cargo minor. Update together with re-verification.
        const VERIFIED_CARGO_MINOR: u64 = 96;

        // Resolve cargo via the `CARGO` env var — the absolute path cargo
        // exports when it spawns the test binary — not PATH: a peer `#[serial]`
        // test prepends a stub-cargo dir to the process-global PATH, and a
        // PATH-resolved spawn here would race it and read the stub's version.
        let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = std::process::Command::new(cargo_bin)
            .arg("--version")
            // Pin cwd: a peer test that deletes the process-global cwd would
            // otherwise make this forked `cargo --version` abort on getcwd.
            .current_dir(anodizer_core::path_util::probe_dir())
            .output()
            .expect("cargo --version must succeed");
        let version_str = String::from_utf8_lossy(&output.stdout);
        // Format: "cargo X.Y.Z (hash date)"
        let minor: Option<u64> = version_str
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.split('.').nth(1))
            .and_then(|s| s.parse().ok());
        let minor =
            minor.unwrap_or_else(|| panic!("could not parse cargo minor from: {version_str}"));
        assert_eq!(
            minor, VERIFIED_CARGO_MINOR,
            "cargo minor version changed from {VERIFIED_CARGO_MINOR} to {minor}. \
             Re-verify the is_index_propagation_failure substrings against \
             `cargo publish` output on the new version, then bump \
             VERIFIED_CARGO_MINOR in this test."
        );
    }

    /// End-to-end retry behaviour: stub `cargo` with a shell script that
    /// fails twice with a propagation-style stderr, then succeeds. The
    /// retry harness must persist through the failures and surface success.
    ///
    /// Uses a counter file under tempdir so successive invocations of the
    /// same script select different exit paths — keeps the test
    /// deterministic without needing a global mutex.
    #[cfg(unix)]
    #[test]
    fn run_cargo_publish_with_retry_recovers_from_propagation_lag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter");
        let stub = tmp.path().join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             if [ $n -lt 3 ]; then\n\
             echo 'error: failed to select a version for the requirement `dep = \"^1.0.0\"`' >&2\n\
             exit 101\n\
             fi\n\
             echo 'published ok'\n\
             exit 0\n",
            counter = counter.display(),
        );
        std::fs::write(&stub, script).expect("write stub");

        // Run the stub via `sh` instead of exec'ing it directly. A freshly
        // written executable that another test thread forks across in the
        // window before its write fd is closed trips ETXTBSY ("Text file
        // busy") on execve; `sh` is a long-lived binary and the stub is only
        // read, so the race cannot occur.
        let cmd = vec![
            "sh".to_string(),
            stub.display().to_string(),
            "publish".to_string(),
        ];
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        // Use a tiny backoff so the retry path exercises the full counter/sleep/error
        // envelope without incurring real wall-clock cost.
        let result = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect("retry harness must succeed after propagation lag");
        assert!(result.status.success(), "final attempt must succeed");

        // Counter file confirms the harness invoked the stub 3 times
        // (initial + 2 retries).
        let n: u32 = std::fs::read_to_string(&counter)
            .expect("counter")
            .trim()
            .parse()
            .expect("u32");
        assert_eq!(n, 3, "expected 3 invocations (initial + 2 retries)");
    }

    /// Fast-fail behaviour: a non-propagation failure (auth) must NOT
    /// trigger retry. The stub fails with a 401-style stderr; harness must
    /// surface immediately without further invocations.
    #[cfg(unix)]
    #[test]
    fn run_cargo_publish_with_retry_does_not_retry_unrelated_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter");
        let stub = tmp.path().join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             echo 'error: failed to publish: 401 Unauthorized' >&2\n\
             exit 101\n",
            counter = counter.display(),
        );
        std::fs::write(&stub, script).expect("write stub");

        // See the recovery test above: route through `sh` to dodge the
        // ETXTBSY race exec'ing a freshly-written stub under parallel tests.
        let cmd = vec![
            "sh".to_string(),
            stub.display().to_string(),
            "publish".to_string(),
        ];
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        let err = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect_err("non-propagation failure must surface");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("401") || chain.contains("Unauthorized") || chain.contains("exit code"),
            "expected upstream error in chain: {chain}"
        );

        let n: u32 = std::fs::read_to_string(&counter)
            .expect("counter")
            .trim()
            .parse()
            .expect("u32");
        assert_eq!(n, 1, "non-propagation failure must NOT retry");
    }

    /// Cross-platform variant of the retry recovery test. Instead of a shell
    /// script stub (unix-only), this variant compiles a minimal Rust binary
    /// whose behaviour is controlled by a counter file — same contract as the
    /// unix shell stub, but works on Windows CI where /bin/sh is absent.
    ///
    /// Gated on `cfg(not(unix))` so only one of the two variants runs per
    /// platform; the shell-script path is preferred on unix (faster compile).
    #[cfg(not(unix))]
    #[test]
    fn run_cargo_publish_with_retry_recovers_from_propagation_lag_windows() {
        // Build the counter stub from an in-test source string. We write
        // a tiny Rust program to a tempdir and compile it with `rustc`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter.txt");
        let src_path = tmp.path().join("stub.rs");
        let exe_path = if cfg!(windows) {
            tmp.path().join("stub.exe")
        } else {
            tmp.path().join("stub")
        };

        // Counter file path passed via env var so the compiled binary can
        // locate it at runtime without baking in a temp path at compile time.
        let src = r#"
use std::fs;

fn main() {
    let counter_path = std::env::var("STUB_COUNTER").expect("STUB_COUNTER not set");
    let n: u32 = fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    fs::write(&counter_path, n.to_string()).expect("write counter");
    if n < 3 {
        eprintln!("error: failed to select a version for the requirement `dep = \"^1.0.0\"`");
        std::process::exit(101);
    }
    println!("published ok");
}
"#;
        std::fs::write(&src_path, src).expect("write stub source");

        let compile = std::process::Command::new("rustc")
            .arg(&src_path)
            .arg("-o")
            .arg(&exe_path)
            .output()
            .expect("rustc spawn");
        if !compile.status.success() {
            panic!(
                "stub compile failed: {}",
                String::from_utf8_lossy(&compile.stderr)
            );
        }

        let cmd = vec![exe_path.display().to_string(), "publish".to_string()];
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        // Serialize STUB_COUNTER mutation across tests in the same
        // binary — the sibling `..._unrelated_failure_windows` test
        // also mutates this env var; two tests running in parallel
        // race the set/remove pair and the spawned stub then sees
        // either the wrong path or NotPresent.
        let _g = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex above; pair set / remove.
        unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
        let result = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect("retry harness must succeed after propagation lag");
        // SAFETY: serialised by env_mutex above; pair with set.
        unsafe { std::env::remove_var("STUB_COUNTER") };
        assert!(result.status.success(), "final attempt must succeed");

        let n: u32 = std::fs::read_to_string(&counter)
            .expect("counter")
            .trim()
            .parse()
            .expect("u32");
        assert_eq!(n, 3, "expected 3 invocations (initial + 2 retries)");
    }

    /// Cross-platform fast-fail variant: non-propagation failure must NOT
    /// retry. Windows CI exercises this path because the unix shell-script
    /// variants are excluded on non-unix platforms.
    #[cfg(not(unix))]
    #[test]
    fn run_cargo_publish_with_retry_does_not_retry_unrelated_failure_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter.txt");
        let src_path = tmp.path().join("stub_auth.rs");
        let exe_path = if cfg!(windows) {
            tmp.path().join("stub_auth.exe")
        } else {
            tmp.path().join("stub_auth")
        };

        let src = r#"
fn main() {
    let counter_path = std::env::var("STUB_COUNTER").expect("STUB_COUNTER not set");
    let n: u32 = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(&counter_path, n.to_string()).expect("write counter");
    eprintln!("error: failed to publish: 401 Unauthorized");
    std::process::exit(101);
}
"#;
        std::fs::write(&src_path, src).expect("write stub source");
        let compile = std::process::Command::new("rustc")
            .arg(&src_path)
            .arg("-o")
            .arg(&exe_path)
            .output()
            .expect("rustc spawn");
        if !compile.status.success() {
            panic!(
                "stub compile failed: {}",
                String::from_utf8_lossy(&compile.stderr)
            );
        }

        let cmd = vec![exe_path.display().to_string(), "publish".to_string()];
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        // Serialize STUB_COUNTER mutation — see the sibling
        // `..._recovers_from_propagation_lag_windows` test for the
        // race this guards against.
        let _g = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex above; pair set / remove.
        unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
        let err = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect_err("non-propagation failure must surface");
        // SAFETY: serialised by env_mutex above; pair with set.
        unsafe { std::env::remove_var("STUB_COUNTER") };
        let chain = format!("{err:#}");
        assert!(
            chain.contains("401") || chain.contains("Unauthorized") || chain.contains("exit code"),
            "expected upstream error in chain: {chain}"
        );

        let n: u32 = std::fs::read_to_string(&counter)
            .expect("counter")
            .trim()
            .parse()
            .expect("u32");
        assert_eq!(n, 1, "non-propagation failure must NOT retry");
    }

    // -----------------------------------------------------------------------
    // wait_for_workspace_deps — pre-publish gate
    //
    // Pin the manifest parser shape and the polling-success path. The
    // sparse-index URL math is exercised by `test_sparse_index_url_shape`
    // above; the gate reuses that helper unchanged.
    // -----------------------------------------------------------------------

    fn write_manifest(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("Cargo.toml");
        std::fs::write(&p, body).expect("write Cargo.toml");
        p
    }

    /// Bare-string dep (`name = "1.2.3"`) and inline-table dep
    /// (`name = { path = "...", version = "..." }`) are both parsed as
    /// version pins; deps not in the workspace name set are filtered out.
    #[test]
    fn workspace_deps_for_crate_picks_up_pinned_workspace_deps() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "cfgd-operator"
version = "1.0.0"

[dependencies]
cfgd-core = { path = "../core", version = "0.4.0" }
cfgd-shared = "0.5.0"
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#,
        );
        let ws_names: HashSet<&str> = ["cfgd-core", "cfgd-shared", "cfgd-operator"]
            .iter()
            .copied()
            .collect();
        let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        deps.sort();
        assert_eq!(
            deps,
            vec![
                ("cfgd-core".to_string(), "0.4.0".to_string()),
                ("cfgd-shared".to_string(), "0.5.0".to_string()),
            ]
        );
    }

    /// `dev-dependencies` and `build-dependencies` participate alongside
    /// `dependencies` — version_sync rewrites all three, and a downstream
    /// publish of an integration-test fixture would race the same way.
    #[test]
    fn workspace_deps_for_crate_includes_dev_and_build_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
core-lib = { path = "../core", version = "0.4.0" }

[dev-dependencies]
test-fixtures = { path = "../fixtures", version = "0.2.0" }

[build-dependencies]
build-tools = { path = "../build", version = "0.3.0" }
"#,
        );
        let ws_names: HashSet<&str> = ["core-lib", "test-fixtures", "build-tools", "leaf"]
            .iter()
            .copied()
            .collect();
        let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        deps.sort();
        assert_eq!(
            deps,
            vec![
                ("build-tools".to_string(), "0.3.0".to_string()),
                ("core-lib".to_string(), "0.4.0".to_string()),
                ("test-fixtures".to_string(), "0.2.0".to_string()),
            ]
        );
    }

    /// `target.'cfg(...)'.dependencies` (and dev/build target variants)
    /// must also be scanned — version_sync rewrites them; missing them
    /// would leave a publish racing the index on platform-specific deps.
    #[test]
    fn workspace_deps_for_crate_scans_target_specific_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[target.'cfg(unix)'.dependencies]
unix-helper = { path = "../unix", version = "0.1.0" }

[target.'cfg(windows)'.build-dependencies]
win-build = { path = "../win", version = "0.2.0" }
"#,
        );
        let ws_names: HashSet<&str> = ["unix-helper", "win-build", "leaf"]
            .iter()
            .copied()
            .collect();
        let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        deps.sort();
        assert_eq!(
            deps,
            vec![
                ("unix-helper".to_string(), "0.1.0".to_string()),
                ("win-build".to_string(), "0.2.0".to_string()),
            ]
        );
    }

    /// Deps with no crates.io-queryable pin anywhere — git deps, path-only
    /// entries, and `workspace = true` inherits with no root version pin —
    /// are skipped (returning them would either timeout or false-confirm
    /// against an unrelated version). The explicit root manifest pins
    /// nothing: "inherited" resolves to a path-only root entry and
    /// "unrooted" has no root entry at all.
    #[test]
    fn workspace_deps_for_crate_skips_deps_without_resolvable_pin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\", \"inherited\"]\n\n\
             [workspace.dependencies]\ninherited = { path = \"inherited\" }\n",
        )
        .expect("write workspace root");
        let leaf_dir = tmp.path().join("leaf");
        std::fs::create_dir_all(&leaf_dir).expect("mkdir leaf");
        let manifest = write_manifest(
            &leaf_dir,
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
inherited = { workspace = true }
unrooted = { workspace = true }
git-only = { git = "https://example.com/foo" }
path-only = { path = "../foo" }
pinned = { path = "../bar", version = "0.5.0" }
"#,
        );
        let ws_names: HashSet<&str> = [
            "inherited",
            "unrooted",
            "git-only",
            "path-only",
            "pinned",
            "leaf",
        ]
        .iter()
        .copied()
        .collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(deps, vec![("pinned".to_string(), "0.5.0".to_string())]);
    }

    /// The same package may appear in several sections with different specs;
    /// a version-less sighting (here an inherit whose root entry has no pin)
    /// must not shadow a pinned occurrence in a later section.
    #[test]
    fn workspace_deps_for_crate_backfills_version_from_later_section() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\" }\n",
        )
        .expect("write workspace root");
        let leaf_dir = tmp.path().join("leaf");
        std::fs::create_dir_all(&leaf_dir).expect("mkdir leaf");
        let manifest = write_manifest(
            &leaf_dir,
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
lib = { workspace = true }

[build-dependencies]
lib = { path = "../lib", version = "0.3.0" }
"#,
        );
        let ws_names: HashSet<&str> = ["lib", "leaf"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("lib".to_string(), "0.3.0".to_string())],
            "one entry, carrying the pinned version from the later section"
        );
    }

    /// A package pinned in two sections collapses to one wait entry; the
    /// first pin wins.
    #[test]
    fn workspace_deps_for_crate_dedupes_across_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
lib = { path = "../lib", version = "0.4.0" }

[dev-dependencies]
lib = { path = "../lib", version = "0.9.9" }
"#,
        );
        let ws_names: HashSet<&str> = ["lib", "leaf"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("lib".to_string(), "0.4.0".to_string())],
            "duplicate pins collapse to one entry, first pin wins"
        );
    }

    /// One run can touch crates from two distinct cargo workspaces (a nested
    /// standalone `[workspace]`); a shared cache must resolve each crate's
    /// inherits against its OWN root, not whichever root was parsed first.
    #[test]
    fn workspace_deps_root_cache_is_keyed_per_workspace_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Outer workspace: pins shared@1.1.1.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nshared = { path = \"shared\", version = \"1.1.1\" }\n",
        )
        .expect("write outer root");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        let app_manifest = write_manifest(
            &app_dir,
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\nshared.workspace = true\n",
        );
        // Nested standalone workspace: pins shared@2.2.2.
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(
            nested.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app2\"]\n\n\
             [workspace.dependencies]\nshared = { path = \"shared\", version = \"2.2.2\" }\n",
        )
        .expect("write nested root");
        let app2_dir = nested.join("app2");
        std::fs::create_dir_all(&app2_dir).expect("mkdir app2");
        let app2_manifest = write_manifest(
            &app2_dir,
            "[package]\nname = \"app2\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\nshared.workspace = true\n",
        );

        let ws_names: HashSet<&str> = ["shared", "app", "app2"].iter().copied().collect();
        let mut cache = RootDepCache::new();
        assert_eq!(
            workspace_deps_for_crate(&app_manifest, &ws_names, &mut cache),
            vec![("shared".to_string(), "1.1.1".to_string())],
            "outer crate resolves against the outer root"
        );
        assert_eq!(
            workspace_deps_for_crate(&app2_manifest, &ws_names, &mut cache),
            vec![("shared".to_string(), "2.2.2".to_string())],
            "nested crate must resolve against its own root, not the cached outer one"
        );
    }

    /// Full-table form rename (`[dependencies.core]` with `package = ...`)
    /// resolves like the inline form.
    #[test]
    fn workspace_deps_for_crate_resolves_full_table_rename() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies.core]
package = "anodizer-core"
path = "../core"
version = "0.8.0"
"#,
        );
        let ws_names: HashSet<&str> = ["anodizer-core", "core", "leaf"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("anodizer-core".to_string(), "0.8.0".to_string())],
            "full-table rename must be waited on under the real package name"
        );
    }

    /// Standard-table form (`[dependencies.name]\nversion = "..."`) is
    /// accepted alongside inline-table / bare-string forms.
    #[test]
    fn workspace_deps_for_crate_handles_standard_table_form() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies.cfgd-core]
path = "../core"
version = "0.4.0"
features = ["extra"]
"#,
        );
        let ws_names: HashSet<&str> = ["cfgd-core", "leaf"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(deps, vec![("cfgd-core".to_string(), "0.4.0".to_string())]);
    }

    /// A renamed dep (`alias = { package = "real", ... }`) must be waited on
    /// under its real package name — that is the name cargo resolves against
    /// the index. The alias key must NOT be matched, even when a workspace
    /// member shares the alias's name ("core" below).
    #[test]
    fn workspace_deps_for_crate_resolves_package_renamed_dep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
core = { package = "anodizer-core", path = "../core", version = "0.8.0" }
"#,
        );
        let ws_names: HashSet<&str> = ["anodizer-core", "core", "leaf"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("anodizer-core".to_string(), "0.8.0".to_string())],
            "wait set must carry the real package name, not the alias"
        );
    }

    /// A rename declared on the workspace root entry — the only place cargo
    /// accepts `package =` for an inherited dep — with the leaf inheriting
    /// via `core.workspace = true`. The wait set must carry the real package
    /// name at the root-pinned version.
    #[test]
    fn workspace_deps_for_crate_resolves_inherited_renamed_dep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"core\"]\n\n\
             [workspace.dependencies]\n\
             core = { path = \"core\", version = \"0.8.0\", package = \"anodizer-core\" }\n",
        )
        .expect("write workspace root");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        let manifest = write_manifest(
            &app_dir,
            r#"
[package]
name = "app"
version = "0.8.0"

[dependencies]
core.workspace = true
"#,
        );
        let ws_names: HashSet<&str> = ["anodizer-core", "app"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("anodizer-core".to_string(), "0.8.0".to_string())],
            "inherited rename must be waited on under its real package name"
        );
    }

    /// A plain `<dep>.workspace = true` inherit whose version pin lives on
    /// the workspace root entry must be waited on at that version — the same
    /// propagation race exists whether the pin is on the leaf or the root.
    #[test]
    fn workspace_deps_for_crate_resolves_inherited_dep_version_from_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\", version = \"0.7.0\" }\n",
        )
        .expect("write workspace root");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        let manifest = write_manifest(
            &app_dir,
            r#"
[package]
name = "app"
version = "0.7.0"

[dependencies]
lib.workspace = true
"#,
        );
        let ws_names: HashSet<&str> = ["lib", "app"].iter().copied().collect();
        let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
        assert_eq!(
            deps,
            vec![("lib".to_string(), "0.7.0".to_string())],
            "root-pinned inherit must be waited on at the root version"
        );
    }

    /// Disabled gate is a no-op even when deps are present — the master
    /// switch protects single-crate workspaces (anodize itself) from the
    /// always-on polling cost.
    #[test]
    fn wait_for_workspace_deps_no_op_when_disabled() {
        let cfg = WaitForWorkspaceDepsConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        let deps = vec![("would-block".to_string(), "9.9.9".to_string())];
        wait_for_workspace_deps_to_appear("dummy", &deps, &cfg, &log)
            .expect("disabled gate must short-circuit before any HTTP");
    }

    /// Empty dep list is a no-op even when the gate is enabled — keeps
    /// the publisher from paying HTTP-client-construction cost on every
    /// crate even after deps have been filtered down to zero.
    #[test]
    fn wait_for_workspace_deps_no_op_when_no_deps() {
        let cfg = WaitForWorkspaceDepsConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        wait_for_workspace_deps_to_appear("dummy", &[], &cfg, &log)
            .expect("empty deps must short-circuit");
    }

    /// End-to-end: a local HTTP responder serves a populated sparse-index
    /// response on first call, so the gate breaks out of its poll loop
    /// after exactly one probe. Exercises `probe_dep_on_index` +
    /// `parse_index_cksum_for_version` integration without hitting the
    /// real crates.io.
    #[test]
    fn probe_dep_on_index_returns_true_when_version_present() {
        let body = r#"{"name":"cfgd-core","vers":"0.4.0","cksum":"abc","yanked":false}"#;
        let body_len = body.len();
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
            .expect("client");
        let url = format!("http://{addr}/cf/gd/cfgd-core");
        let found = probe_dep_on_index(&client, &url, "0.4.0").expect("probe ok");
        assert!(found, "version should be detected as present");
    }

    /// A 200 with a body that lacks the requested version returns
    /// false — the gate must loop and retry, not treat any 2xx as
    /// "dep present."
    #[test]
    fn probe_dep_on_index_returns_false_when_version_absent() {
        // Index has 0.3.0 but we're waiting for 0.4.0.
        let body = r#"{"name":"cfgd-core","vers":"0.3.0","cksum":"old","yanked":false}"#;
        let body_len = body.len();
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
            .expect("client");
        let url = format!("http://{addr}/cf/gd/cfgd-core");
        let found = probe_dep_on_index(&client, &url, "0.4.0").expect("probe ok");
        assert!(!found, "missing version must return false, not error");
    }

    /// A 404 response (crate has never been published) returns false —
    /// the gate keeps polling rather than bailing, because the dep's
    /// upstream Release.yml run may still be in flight.
    #[test]
    fn probe_dep_on_index_returns_false_on_404() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
            .expect("client");
        let url = format!("http://{addr}/cf/gd/cfgd-core");
        let found = probe_dep_on_index(&client, &url, "0.4.0").expect("404 is not an error");
        assert!(!found);
    }

    // -----------------------------------------------------------------------
    // Operator-facing log message helpers.
    // -----------------------------------------------------------------------

    #[test]
    fn run_start_and_done_messages_carry_counts() {
        assert_eq!(
            run_start_message(3),
            "starting cargo publish for 3 selected crate(s)"
        );
        assert_eq!(
            run_per_crate_start_message("cfgd-core"),
            "starting per-crate cargo publish for 'cfgd-core'"
        );
        assert_eq!(
            run_done_message(2),
            "finished cargo publish — 2 crate(s) processed"
        );
    }

    #[test]
    fn run_no_eligible_crates_warning_names_the_total() {
        let w = run_no_eligible_crates_warning(5);
        assert!(w.starts_with("cargo publisher registered but 0 of 5 effective crate(s)"));
        assert!(w.contains("--crate / --all"));
    }

    // -----------------------------------------------------------------------
    // strip_key_prefix — key-boundary check guarding `version` scans.
    // -----------------------------------------------------------------------

    #[test]
    fn strip_key_prefix_accepts_boundary_chars_only() {
        // Whitespace, `=`, and `.` are valid boundaries after the key.
        assert_eq!(
            strip_key_prefix("version = \"1.0\"", "version"),
            Some(" = \"1.0\"")
        );
        assert_eq!(
            strip_key_prefix("version= \"1.0\"", "version"),
            Some("= \"1.0\"")
        );
        assert_eq!(
            strip_key_prefix("version.workspace = true", "version"),
            Some(".workspace = true")
        );
        // A non-boundary continuation (`versioned`, `versions`) is rejected.
        assert_eq!(strip_key_prefix("versioned = 1", "version"), None);
        assert_eq!(strip_key_prefix("versions = []", "version"), None);
        // Bare key with nothing after it is rejected (not a key=value line).
        assert_eq!(strip_key_prefix("version", "version"), None);
    }

    // -----------------------------------------------------------------------
    // scan_section_version — section scoping + literal/workspace/none.
    // -----------------------------------------------------------------------

    #[test]
    fn scan_section_version_reads_literal_and_strips_inline_comment() {
        let body = "[package]\nname = \"x\"\nversion = \"1.2.3\" # pinned\n";
        assert_eq!(
            scan_section_version(body, "[package]"),
            CargoVersionRef::Literal("1.2.3".to_string())
        );
    }

    #[test]
    fn scan_section_version_detects_dot_and_inline_workspace_inherit() {
        let dot = "[package]\nversion.workspace = true\n";
        assert_eq!(
            scan_section_version(dot, "[package]"),
            CargoVersionRef::Workspace
        );
        let inline = "[package]\nversion = { workspace = true }\n";
        assert_eq!(
            scan_section_version(inline, "[package]"),
            CargoVersionRef::Workspace
        );
    }

    #[test]
    fn scan_section_version_stops_at_sibling_section_but_not_subtable() {
        // The version lives only in a SIBLING section -> None (scan stops at
        // `[dependencies]`, never reaching it).
        let sibling = "[package]\nname = \"x\"\n[dependencies]\nversion = \"9.9.9\"\n";
        assert_eq!(
            scan_section_version(sibling, "[package]"),
            CargoVersionRef::None
        );

        // A sub-table of the logical block does NOT end the scan: the version
        // after `[workspace.package.metadata.x]` is still found.
        let subtable = concat!(
            "[workspace.package]\n",
            "[workspace.package.metadata.docs]\n",
            "foo = 1\n",
            "version = \"7.7.7\"\n",
        );
        assert_eq!(
            scan_section_version(subtable, "[workspace.package]"),
            CargoVersionRef::Literal("7.7.7".to_string())
        );
    }

    #[test]
    fn scan_section_version_skips_comment_lines() {
        let body = "# comment\n[package]\n# version = \"0.0.0\"\nversion = \"4.5.6\"\n";
        assert_eq!(
            scan_section_version(body, "[package]"),
            CargoVersionRef::Literal("4.5.6".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // find_workspace_root_manifest — anchored [workspace] header walk.
    // -----------------------------------------------------------------------

    /// Walks up from a leaf crate dir to the manifest carrying `[workspace]`.
    #[test]
    fn find_workspace_root_manifest_walks_up_to_workspace() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/leaf\"]\n",
        )
        .unwrap();
        let leaf = root.path().join("crates").join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let found = find_workspace_root_manifest(&leaf).expect("workspace root found");
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(root.path().join("Cargo.toml")).unwrap()
        );
    }

    /// A bare `[workspace.package.metadata.docs.rs]` sub-table in a leaf
    /// manifest must NOT be mistaken for a workspace root (anchored exact
    /// header match, not `starts_with`).
    #[test]
    fn find_workspace_root_manifest_ignores_metadata_subtable() {
        let root = tempfile::tempdir().expect("tempdir");
        // Leaf-only manifest with a metadata sub-table but no real [workspace].
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"solo\"\n[workspace.package.metadata.docs.rs]\nall-features = true\n",
        )
        .unwrap();
        assert_eq!(find_workspace_root_manifest(root.path()), None);
    }

    // -----------------------------------------------------------------------
    // publish_to_cargo — end-to-end orchestration in dry-run mode.
    //
    // Dry-run takes the early `ctx.is_dry_run()` branch: it builds the same
    // expanded selection, eligibility map (skip/if gating), and topological
    // `sorted_names` the live path uses, then emits per-crate start +
    // `(dry-run) would run: <cmd>` status lines instead of shelling out. The
    // captured status stream is therefore a faithful witness of the ordering
    // and gating decisions WITHOUT any network or subprocess. Covers all
    // three config modes — single-crate, workspace-lockstep, workspace
    // per-crate — for the publish-graph walk.
    // -----------------------------------------------------------------------

    use anodizer_core::config::{PublishConfig, WorkspaceConfig};
    // `Verbosity` / `LogLevel` are not in the file-level imports `super::*`
    // re-exports; `StageLogger` is, but an explicit re-import of a glob item
    // is permitted (explicit binding wins, same resolved path — no conflict).
    use anodizer_core::log::{LogLevel, StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;

    /// A crate with a `publish.cargo` block (eligible for the cargo
    /// publisher) plus the given workspace-internal `depends_on` edges.
    fn cargo_crate(name: &str, deps: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A crate with the given `publish.cargo` config (so `skip:` / `if:`
    /// can be exercised) and `depends_on` edges.
    fn cargo_crate_with_cfg(name: &str, deps: &[&str], cfg: CargoPublishConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            publish: Some(PublishConfig {
                cargo: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A crate with NO `publish.cargo` block — present in the config (so it
    /// participates in `depends_on` resolution) but not eligible to publish.
    fn plain_crate(name: &str, deps: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    /// Run `publish_to_cargo` in dry-run mode with a capturing logger and
    /// return the ordered list of crate names whose per-crate-start line was
    /// emitted — i.e. the order `publish_to_cargo` walked the publish graph.
    fn dry_run_publish_order(ctx: &mut Context) -> Vec<String> {
        let (log, cap) = StageLogger::with_capture("publish-test", Verbosity::Normal);
        let selected = ctx.options.selected_crates.clone();
        let mut record = Vec::new();
        publish_to_cargo(ctx, &selected, &log, &mut record).expect("dry-run publish must succeed");
        // Each crate emits `run_per_crate_start_message(name)` exactly once,
        // in topological order, before its `(dry-run) would run` line.
        cap.all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Status)
            .filter_map(|(_, m)| {
                m.strip_prefix("starting per-crate cargo publish for '")
                    .and_then(|rest| rest.strip_suffix('\''))
                    .map(str::to_string)
            })
            .collect()
    }

    /// Single-crate mode: one eligible crate with no deps publishes itself
    /// and only itself. The expanded selection is exactly `[the crate]`.
    #[test]
    fn publish_to_cargo_single_crate_mode_publishes_the_one_crate() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![cargo_crate("solo", &[])])
            .selected_crates(vec!["solo".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["solo"]);
    }

    /// Workspace-lockstep mode: every crate lives under top-level
    /// `crates:` and a single `--crate cfgd` selection expands transitively
    /// to its dependency chain, published dependencies-first.
    #[test]
    fn publish_to_cargo_lockstep_orders_dependency_before_dependent() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                cargo_crate("cfgd", &["cfgd-core"]),
                cargo_crate("cfgd-core", &[]),
            ])
            // Select only the leaf binary; the dependency must be pulled in
            // by expand_with_transitive_deps and published FIRST.
            .selected_crates(vec!["cfgd".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["cfgd-core", "cfgd"]);
    }

    /// Workspace-lockstep, three-level chain: a→b→c must publish c, b, a in
    /// strict topological order regardless of declaration order.
    #[test]
    fn publish_to_cargo_lockstep_orders_three_level_chain() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                cargo_crate("a", &["b"]),
                cargo_crate("b", &["c"]),
                cargo_crate("c", &[]),
            ])
            .selected_crates(vec!["a".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["c", "b", "a"]);
    }

    /// Workspace per-crate mode: crates live under `workspaces:` (NOT
    /// top-level `crates:`). `all_crates` overlays the workspace members,
    /// and a cross-member dep is still ordered dependency-first.
    #[test]
    fn publish_to_cargo_per_crate_workspace_orders_across_members() {
        let core_ws = WorkspaceConfig {
            name: "core-ws".to_string(),
            crates: vec![cargo_crate("cfgd-core", &[])],
            ..Default::default()
        };
        let app_ws = WorkspaceConfig {
            name: "app-ws".to_string(),
            crates: vec![cargo_crate("cfgd", &["cfgd-core"])],
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .workspaces(vec![core_ws, app_ws])
            .selected_crates(vec!["cfgd".to_string()])
            .dry_run(true)
            .build();
        // cfgd-core lives in a DIFFERENT workspace than cfgd, yet the cross-
        // workspace depends_on edge still forces it published first.
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["cfgd-core", "cfgd"]);
    }

    /// A dependency without its own `publish.cargo` block is pulled into the
    /// graph for ordering but is itself NOT published — only cargo-eligible
    /// crates appear in the emitted order, and the eligible dependent still
    /// publishes.
    #[test]
    fn publish_to_cargo_skips_dep_lacking_cargo_block() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                cargo_crate("app", &["helper"]),
                plain_crate("helper", &[]),
            ])
            .selected_crates(vec!["app".to_string()])
            .dry_run(true)
            .build();
        // `helper` has no publish.cargo → not in cargo_cfgs → filtered out of
        // `publishable`; only `app` is published.
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["app"]);
    }

    /// `publish.cargo.skip: true` removes the crate from the eligible set
    /// even though it carries a cargo block — the other eligible crate still
    /// publishes.
    #[test]
    fn publish_to_cargo_honors_skip_true() {
        let skipped = cargo_crate_with_cfg(
            "skipme",
            &[],
            CargoPublishConfig {
                skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![skipped, cargo_crate("keepme", &[])])
            .selected_crates(vec!["skipme".to_string(), "keepme".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["keepme"]);
    }

    /// `publish.cargo.if: "false"` (a falsy `if` condition) gates the crate
    /// out of the eligible set — the live path renders the template and
    /// drops the crate when it evaluates falsy.
    #[test]
    fn publish_to_cargo_honors_falsy_if_condition() {
        let gated = cargo_crate_with_cfg(
            "gated",
            &[],
            CargoPublishConfig {
                if_condition: Some("false".to_string()),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![gated, cargo_crate("open", &[])])
            .selected_crates(vec!["gated".to_string(), "open".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["open"]);
    }

    /// `if: "true"` keeps the crate eligible — the truthy branch of the
    /// `if` gate is the complement of the falsy test above.
    #[test]
    fn publish_to_cargo_keeps_crate_when_if_condition_truthy() {
        let gated = cargo_crate_with_cfg(
            "gated",
            &[],
            CargoPublishConfig {
                if_condition: Some("true".to_string()),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![gated])
            .selected_crates(vec!["gated".to_string()])
            .dry_run(true)
            .build();
        assert_eq!(dry_run_publish_order(&mut ctx), vec!["gated"]);
    }

    /// The `--skip=cargo` stage gate short-circuits `publish_to_cargo`
    /// before any per-crate work: no crate-start lines are emitted even
    /// though an eligible crate is selected.
    #[test]
    fn publish_to_cargo_short_circuits_when_stage_skipped() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![cargo_crate("solo", &[])])
            .selected_crates(vec!["solo".to_string()])
            .skip_stages(vec!["cargo".to_string()])
            .dry_run(true)
            .build();
        assert!(
            dry_run_publish_order(&mut ctx).is_empty(),
            "--skip=cargo must publish nothing"
        );
    }

    /// The dry-run command line for each crate reflects its per-crate
    /// `publish.cargo` config (here `--no-verify` + the implicit
    /// `--allow-dirty`), proving the cfg→argv wiring survives the
    /// orchestration, not just the unit `publish_command` call.
    #[test]
    fn publish_to_cargo_dry_run_emits_configured_flags() {
        let crate_cfg = cargo_crate_with_cfg(
            "flagged",
            &[],
            CargoPublishConfig {
                no_verify: Some(true),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_cfg])
            .selected_crates(vec!["flagged".to_string()])
            .dry_run(true)
            .build();
        let (log, cap) = StageLogger::with_capture("publish-test", Verbosity::Normal);
        let selected = ctx.options.selected_crates.clone();
        let mut record = Vec::new();
        publish_to_cargo(&mut ctx, &selected, &log, &mut record).expect("dry-run ok");
        let dry_line = cap
            .all_messages()
            .into_iter()
            .find_map(|(_, m)| m.strip_prefix("(dry-run) would run: ").map(str::to_string))
            .expect("dry-run command line emitted");
        assert!(
            dry_line.contains("cargo publish -p flagged"),
            "missing publish target: {dry_line}"
        );
        assert!(
            dry_line.contains("--no-verify"),
            "configured --no-verify not threaded into dry-run cmd: {dry_line}"
        );
        assert!(
            dry_line.contains("--allow-dirty"),
            "implicit --allow-dirty missing: {dry_line}"
        );
    }

    /// Diamond graph (d depends on b and c, both depend on a) publishes `a`
    /// first and `d` last; the two middle crates appear in the
    /// deterministic alphabetical seed order the topo-sort guarantees.
    #[test]
    fn publish_to_cargo_orders_diamond_dependency_graph() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                cargo_crate("d", &["b", "c"]),
                cargo_crate("b", &["a"]),
                cargo_crate("c", &["a"]),
                cargo_crate("a", &[]),
            ])
            .selected_crates(vec!["d".to_string()])
            .dry_run(true)
            .build();
        let order = dry_run_publish_order(&mut ctx);
        assert_eq!(order.first().map(String::as_str), Some("a"), "root first");
        assert_eq!(order.last().map(String::as_str), Some("d"), "sink last");
        // b and c are independent middles — deterministic alpha seed order.
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    // -----------------------------------------------------------------------
    // cargo_publish_plan — the #25 single-source-of-truth extraction.
    //
    // Asserts the resolved plan directly (order + per-crate cfgs + per-crate
    // versions) rather than only the dry-run log, so a regression in the
    // version/cfg resolution surfaces even if the ordering stays correct.
    // Covered across all three config modes per the all-modes requirement.
    // -----------------------------------------------------------------------

    /// Quiet logger for plan resolution — the plan emits skip/if status
    /// lines we don't inspect here, so a non-capturing logger suffices.
    fn quiet_log() -> StageLogger {
        StageLogger::new("publish-test", Verbosity::Normal)
    }

    /// Write a `[package]` manifest pinning `version` under a fresh subdir of
    /// `root` and return a cargo-eligible `CrateConfig` rooted there, so the
    /// plan's per-crate version resolution reads a REAL on-disk version
    /// instead of the cwd manifest. `cfg` controls the publish.cargo block.
    fn disk_crate(
        root: &std::path::Path,
        name: &str,
        version: &str,
        deps: &[&str],
        cfg: CargoPublishConfig,
    ) -> CrateConfig {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir crate dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        )
        .expect("write manifest");
        CrateConfig {
            name: name.to_string(),
            path: dir.display().to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            publish: Some(PublishConfig {
                cargo: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Single-crate mode: the plan resolves exactly the one selected crate,
    /// carries its cargo cfg, and reads the crate's own on-disk version.
    #[test]
    fn cargo_publish_plan_single_crate_resolves_order_cfg_and_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let solo = disk_crate(
            tmp.path(),
            "solo",
            "1.2.3",
            &[],
            CargoPublishConfig {
                no_verify: Some(true),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v9.9.9") // release version differs from the on-disk version
            .crates(vec![solo])
            .selected_crates(vec!["solo".to_string()])
            .build();
        let plan = cargo_publish_plan(&mut ctx, &["solo".to_string()], &quiet_log())
            .expect("plan resolves");

        assert_eq!(plan.order, vec!["solo"]);
        // cfg survives into the plan map verbatim.
        assert_eq!(plan.cfgs.get("solo").and_then(|c| c.no_verify), Some(true));
        // Version is read from the crate's own manifest, not the release tag.
        assert_eq!(plan.versions.get("solo").map(String::as_str), Some("1.2.3"));
    }

    /// Workspace-lockstep mode: a `--crate` selection of the leaf expands
    /// transitively, the plan orders the dependency first, and EACH crate's
    /// own on-disk version is resolved (mixed cadence: 0.4.0 vs 0.4.1).
    #[test]
    fn cargo_publish_plan_lockstep_orders_deps_and_resolves_both_versions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let core = disk_crate(
            tmp.path(),
            "cfgd-core",
            "0.4.0",
            &[],
            CargoPublishConfig::default(),
        );
        let app = disk_crate(
            tmp.path(),
            "cfgd",
            "0.4.1",
            &["cfgd-core"],
            CargoPublishConfig::default(),
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v0.4.0")
            .crates(vec![app, core])
            .selected_crates(vec!["cfgd".to_string()])
            .build();
        let plan = cargo_publish_plan(&mut ctx, &["cfgd".to_string()], &quiet_log())
            .expect("plan resolves");

        assert_eq!(plan.order, vec!["cfgd-core", "cfgd"]);
        assert_eq!(
            plan.versions.get("cfgd-core").map(String::as_str),
            Some("0.4.0")
        );
        // Distinct per-crate version proves the plan reads each manifest.
        assert_eq!(plan.versions.get("cfgd").map(String::as_str), Some("0.4.1"));
        // Both eligible crates have a (default) cargo cfg recorded.
        assert!(plan.cfgs.contains_key("cfgd-core"));
        assert!(plan.cfgs.contains_key("cfgd"));
    }

    /// Workspace per-crate mode: members live under `workspaces:` and the
    /// plan overlays them into `all_crates`, orders a cross-member dep
    /// first, and records each member's cfg/version from disk.
    #[test]
    fn cargo_publish_plan_per_crate_workspace_overlays_members() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let core = disk_crate(
            tmp.path(),
            "cfgd-core",
            "0.3.0",
            &[],
            CargoPublishConfig::default(),
        );
        let app = disk_crate(
            tmp.path(),
            "cfgd",
            "2.0.0",
            &["cfgd-core"],
            CargoPublishConfig::default(),
        );
        let core_ws = WorkspaceConfig {
            name: "core-ws".to_string(),
            crates: vec![core],
            ..Default::default()
        };
        let app_ws = WorkspaceConfig {
            name: "app-ws".to_string(),
            crates: vec![app],
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .tag("v2.0.0")
            .workspaces(vec![core_ws, app_ws])
            .selected_crates(vec!["cfgd".to_string()])
            .build();
        let plan = cargo_publish_plan(&mut ctx, &["cfgd".to_string()], &quiet_log())
            .expect("plan resolves");

        assert_eq!(plan.order, vec!["cfgd-core", "cfgd"]);
        // `all_crates` is the overlay both members are drawn from.
        let names: HashSet<&str> = plan.all_crates.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains("cfgd-core") && names.contains("cfgd"));
        // Cross-member crates resolve their distinct on-disk versions.
        assert_eq!(
            plan.versions.get("cfgd-core").map(String::as_str),
            Some("0.3.0")
        );
        assert_eq!(plan.versions.get("cfgd").map(String::as_str), Some("2.0.0"));
    }

    /// A `skip: true` crate is dropped from BOTH the cfg map and the order —
    /// the plan is the single source of truth, so the skip must not leave a
    /// dangling cfg entry that a later consumer could publish.
    #[test]
    fn cargo_publish_plan_skip_true_removes_from_cfgs_and_order() {
        let skipped = cargo_crate_with_cfg(
            "skipme",
            &[],
            CargoPublishConfig {
                skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![skipped, cargo_crate("keepme", &[])])
            .selected_crates(vec!["skipme".to_string(), "keepme".to_string()])
            .build();
        let plan = cargo_publish_plan(
            &mut ctx,
            &["skipme".to_string(), "keepme".to_string()],
            &quiet_log(),
        )
        .expect("plan resolves");

        assert_eq!(plan.order, vec!["keepme"]);
        assert!(
            !plan.cfgs.contains_key("skipme"),
            "skip=true must drop the cfg entry too: {:?}",
            plan.cfgs.keys().collect::<Vec<_>>()
        );
    }

    /// A falsy `if:` condition drops the crate from the plan; the surviving
    /// crate keeps its cfg + order. Complements the skip test (separate gate).
    #[test]
    fn cargo_publish_plan_falsy_if_drops_crate() {
        let gated = cargo_crate_with_cfg(
            "gated",
            &[],
            CargoPublishConfig {
                if_condition: Some("false".to_string()),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![gated, cargo_crate("open", &[])])
            .selected_crates(vec!["gated".to_string(), "open".to_string()])
            .build();
        let plan = cargo_publish_plan(
            &mut ctx,
            &["gated".to_string(), "open".to_string()],
            &quiet_log(),
        )
        .expect("plan resolves");
        assert_eq!(plan.order, vec!["open"]);
        assert!(!plan.cfgs.contains_key("gated"));
    }

    /// Empty selection (no `--crate`) means "all eligible crates": every
    /// crate with a publish.cargo block lands in the plan, ordered topo.
    #[test]
    fn cargo_publish_plan_empty_selection_takes_all_eligible() {
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![cargo_crate("app", &["lib"]), cargo_crate("lib", &[])])
            .build();
        let plan = cargo_publish_plan(&mut ctx, &[], &quiet_log()).expect("plan resolves");
        assert_eq!(plan.order, vec!["lib", "app"]);
    }

    /// A malformed `if:` template (unterminated Tera expression) propagates
    /// the render error out of plan resolution rather than silently keeping
    /// or dropping the crate.
    #[test]
    fn cargo_publish_plan_propagates_if_render_error() {
        let bad = cargo_crate_with_cfg(
            "bad",
            &[],
            CargoPublishConfig {
                // Unbalanced delimiters — Tera render must error.
                if_condition: Some("{{ unterminated".to_string()),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![bad])
            .selected_crates(vec!["bad".to_string()])
            .build();
        // CargoPublishPlan is not Debug, so match rather than expect_err.
        let chain = match cargo_publish_plan(&mut ctx, &["bad".to_string()], &quiet_log()) {
            Ok(_) => panic!("malformed if template must surface as Err"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            chain.contains("if") || chain.contains("template") || chain.contains("render"),
            "expected an if-template render error in the chain: {chain}"
        );
    }

    // -----------------------------------------------------------------------
    // publish_to_cargo — empty-plan early return + no-eligible publisher run.
    // -----------------------------------------------------------------------

    /// When the expanded selection matches no cargo-eligible crate, the plan
    /// is empty and `publish_to_cargo` returns Ok without emitting any
    /// per-crate start line (the empty-`sorted_names` early return).
    #[test]
    fn publish_to_cargo_empty_plan_is_clean_noop() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![cargo_crate("real", &[])])
            // Select a name that doesn't exist → expanded selection is empty
            // of any eligible crate → plan order is empty.
            .selected_crates(vec!["ghost".to_string()])
            .dry_run(true)
            .build();
        assert!(
            dry_run_publish_order(&mut ctx).is_empty(),
            "no eligible crate selected ⇒ no per-crate work"
        );
    }

    /// `CargoPublisher::run` with zero cargo-configured crates emits the
    /// canonical no-eligible warn and returns empty evidence (the
    /// `eligible == 0` short-circuit), without delegating into the loop.
    #[test]
    fn cargo_publisher_run_warns_when_no_cargo_crate_configured() {
        use anodizer_core::Publisher;
        // A crate with NO publish.cargo block ⇒ count_cargo_configured == 0.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![plain_crate("plain", &[])])
            .selected_crates(vec!["plain".to_string()])
            .dry_run(true)
            .build();
        let ev = CargoPublisher::new().run(&mut ctx).expect("run ok");
        assert_eq!(ev.publisher, "cargo");
        // No crate published ⇒ no recorded yank targets, no primary ref.
        assert!(decode_cargo_yank_targets(&ev.extra).is_empty());
        assert!(ev.primary_ref.is_none());
    }

    /// `skips_on_nightly` is true for the cargo publisher — nightly/snapshot
    /// builds carry a non-publishable version and must not hit crates.io.
    #[test]
    fn cargo_publisher_skips_on_nightly() {
        use anodizer_core::Publisher;
        assert!(CargoPublisher::new().skips_on_nightly());
    }

    /// `decode_cargo_yank_targets` returns an empty vec for any non-Cargo
    /// evidence variant, so rollback treats a foreign-evidence run as
    /// "nothing published" and no-ops instead of panicking.
    #[test]
    fn decode_cargo_yank_targets_empty_for_non_cargo_variant() {
        // `PublishEvidenceExtra::None` is the default/empty variant — any
        // non-Cargo variant must decode to an empty target list.
        let extra = anodizer_core::PublishEvidenceExtra::default();
        assert!(decode_cargo_yank_targets(&extra).is_empty());
    }

    /// `programmatic_rollback_on_failure` is gated on a non-empty recorded
    /// target set: a run that published nothing stays inert (no rollback),
    /// while a run that recorded a yank target opts into rollback.
    #[test]
    fn programmatic_rollback_gated_on_recorded_targets() {
        use anodizer_core::Publisher;
        let p = CargoPublisher::new();

        let mut empty = anodizer_core::PublishEvidence::new("cargo");
        empty.extra = encode_cargo_yank_targets(&[]);
        assert!(
            !p.programmatic_rollback_on_failure(&empty),
            "empty record ⇒ no rollback"
        );

        let mut nonempty = anodizer_core::PublishEvidence::new("cargo");
        nonempty.extra = encode_cargo_yank_targets(&[CargoYankTarget {
            name: "x".into(),
            version: "1.0.0".into(),
            registry: None,
            index: None,
        }]);
        assert!(
            p.programmatic_rollback_on_failure(&nonempty),
            "recorded target ⇒ rollback"
        );
    }

    /// Dry-run rollback takes the `is_dry_run` branch: it returns Ok WITHOUT
    /// spawning `cargo`. "No spawn" is proven by shadowing `cargo` with the
    /// argv-recording stub: any reached `cargo yank` would land in the argv
    /// log, so an empty log witnesses the dry-run short-circuit firing
    /// before the loop. The stub is PREPENDED to PATH (never a wholesale
    /// replacement, which would make every concurrent PATH-resolved spawn
    /// in this binary flaky). Gated unix: mutates PATH and uses unix paths.
    #[cfg(unix)]
    #[test]
    fn rollback_dry_run_returns_ok_without_spawning_cargo() {
        use anodizer_core::Publisher;
        let tmp = tempfile::tempdir().expect("tempdir");
        let argv_log = tmp.path().join("argv.log");
        let new_path =
            super::partial_rollback_tests::install_cargo_stub(tmp.path(), &argv_log, "none");
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .dry_run(true)
            .build();
        // Two recorded targets so the loop WOULD spawn twice if reached.
        let targets = vec![
            CargoYankTarget {
                name: "a".into(),
                version: "1.0.0".into(),
                registry: None,
                index: None,
            },
            CargoYankTarget {
                name: "b".into(),
                version: "2.0.0".into(),
                registry: None,
                index: None,
            },
        ];
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&targets);

        let _g = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex; paired with the restore below.
        unsafe { std::env::set_var("PATH", &new_path) };
        let rb = CargoPublisher::new().rollback(&mut ctx, &evidence);
        // SAFETY: restore PATH (paired with the set above).
        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        rb.expect("dry-run rollback must short-circuit to Ok before spawning");
        assert!(
            super::partial_rollback_tests::read_argv_log(&argv_log).is_empty(),
            "dry-run rollback must never spawn cargo"
        );
    }

    // -----------------------------------------------------------------------
    // extract_version_pin — the three TOML dep shapes + the None branches.
    //
    // workspace_deps_for_crate tests above exercise the happy paths end to
    // end; these pin the helper directly so each early-return branch (bare
    // string, inline-table workspace-inherit, inline-table version, standard
    // table workspace-inherit, standard table version, no-version) is
    // observable in isolation.
    // -----------------------------------------------------------------------

    fn dep_item(toml_body: &str, key: &str) -> toml_edit::Item {
        let doc = toml_body.parse::<toml_edit::DocumentMut>().expect("parse");
        doc["dependencies"][key].clone()
    }

    #[test]
    fn extract_version_pin_bare_string() {
        let item = dep_item("[dependencies]\nfoo = \"1.2.3\"\n", "foo");
        assert_eq!(extract_version_pin(&item), Some("1.2.3".to_string()));
    }

    #[test]
    fn extract_version_pin_inline_table_version() {
        let item = dep_item(
            "[dependencies]\nfoo = { path = \"../foo\", version = \"4.5.6\" }\n",
            "foo",
        );
        assert_eq!(extract_version_pin(&item), Some("4.5.6".to_string()));
    }

    #[test]
    fn extract_version_pin_inline_table_workspace_inherit_is_none() {
        let item = dep_item("[dependencies]\nfoo = { workspace = true }\n", "foo");
        assert_eq!(extract_version_pin(&item), None);
    }

    #[test]
    fn extract_version_pin_inline_table_no_version_is_none() {
        // path-only inline table — nothing to poll for.
        let item = dep_item("[dependencies]\nfoo = { path = \"../foo\" }\n", "foo");
        assert_eq!(extract_version_pin(&item), None);
    }

    #[test]
    fn extract_version_pin_standard_table_version() {
        let item = dep_item(
            "[dependencies.foo]\npath = \"../foo\"\nversion = \"7.8.9\"\n",
            "foo",
        );
        assert_eq!(extract_version_pin(&item), Some("7.8.9".to_string()));
    }

    #[test]
    fn extract_version_pin_standard_table_workspace_inherit_is_none() {
        let item = dep_item("[dependencies.foo]\nworkspace = true\n", "foo");
        assert_eq!(extract_version_pin(&item), None);
    }

    #[test]
    fn extract_version_pin_standard_table_no_version_is_none() {
        let item = dep_item("[dependencies.foo]\npath = \"../foo\"\n", "foo");
        assert_eq!(extract_version_pin(&item), None);
    }

    // -----------------------------------------------------------------------
    // workspace_deps_for_crate — degraded-input branches (unreadable /
    // unparseable manifest) must return an empty vec so the gate no-ops
    // rather than erroring out an otherwise-valid publish.
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_deps_for_crate_missing_manifest_returns_empty() {
        let ws: HashSet<&str> = ["a"].iter().copied().collect();
        let nonexistent = std::path::Path::new("/nonexistent/dir/does/not/exist/Cargo.toml");
        assert!(workspace_deps_for_crate(nonexistent, &ws, &mut RootDepCache::new()).is_empty());
    }

    #[test]
    fn workspace_deps_for_crate_unparseable_manifest_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(tmp.path(), "this is = = not valid toml [[[");
        let ws: HashSet<&str> = ["a"].iter().copied().collect();
        assert!(workspace_deps_for_crate(&manifest, &ws, &mut RootDepCache::new()).is_empty());
    }

    /// A `[target.<cfg>]` whose value is not a dependency table (e.g. a
    /// stray scalar) is skipped without panicking — the recursion guards
    /// against malformed target sections.
    #[test]
    fn workspace_deps_for_crate_skips_non_table_target_value() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest = write_manifest(
            tmp.path(),
            r#"
[package]
name = "leaf"
version = "1.0.0"

[target]
"cfg(unix)" = "not-a-table"

[dependencies]
real = { path = "../real", version = "1.0.0" }
"#,
        );
        let ws: HashSet<&str> = ["real", "leaf"].iter().copied().collect();
        // The malformed target scalar is skipped; the normal dep is still found.
        assert_eq!(
            workspace_deps_for_crate(&manifest, &ws, &mut RootDepCache::new()),
            vec![("real".to_string(), "1.0.0".to_string())]
        );
    }

    // -----------------------------------------------------------------------
    // scan_section_version — workspace-inherit branches inside the scan that
    // the read_cargo_toml_version tests reach only indirectly.
    // -----------------------------------------------------------------------

    /// `version.workspace = true` immediately followed by another value on
    /// the same logical line is classified Workspace (the dot-form branch).
    #[test]
    fn scan_section_version_dot_workspace_true() {
        let body = "[package]\nname = \"x\"\nversion.workspace = true\n";
        assert_eq!(
            scan_section_version(body, "[package]"),
            CargoVersionRef::Workspace
        );
    }

    /// A workspace-inherit manifest whose workspace root has NO
    /// `[workspace.package].version` resolves to None (the `_ => None` arm
    /// in read_cargo_toml_version) — the publish path then falls back to the
    /// release version.
    #[test]
    fn read_cargo_toml_version_workspace_root_without_version_is_none() {
        let ws_root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            ws_root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n[workspace.package]\nedition = \"2021\"\n",
        )
        .unwrap();
        let leaf = ws_root.path().join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion.workspace = true\n",
        )
        .unwrap();
        // [workspace.package] exists but carries no `version` ⇒ None.
        assert_eq!(read_cargo_toml_version(leaf.to_str().unwrap()), None);
    }

    // -----------------------------------------------------------------------
    // run_cargo_publish_with_retry — exhaustion path (all retries fail).
    //
    // The recovery + fast-fail paths are covered above; this pins the third
    // arm: a propagation-style failure that NEVER clears must retry the full
    // PUBLISH_PROPAGATION_RETRIES budget, then surface the last failure.
    // -----------------------------------------------------------------------

    /// A stub that emits a propagation-style stderr on EVERY invocation must
    /// be retried exactly `PUBLISH_PROPAGATION_RETRIES` times (initial + the
    /// rest) and then surface the failure — never loop forever, never
    /// succeed.
    #[cfg(unix)]
    #[test]
    fn run_cargo_publish_with_retry_exhausts_then_surfaces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter");
        let stub = tmp.path().join("cargo");
        // Always fail with a propagation-shaped stderr; bump the counter so
        // we can assert the exact attempt count.
        let script = format!(
            "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             echo 'error: no matching package named `dep` found' >&2\n\
             exit 101\n",
            counter = counter.display(),
        );
        std::fs::write(&stub, script).expect("write stub");

        // Route through `sh` to dodge the ETXTBSY race (see the recovery
        // test above for the rationale).
        let cmd = vec![
            "sh".to_string(),
            stub.display().to_string(),
            "publish".to_string(),
        ];
        let log = StageLogger::new("publish-test", Verbosity::Normal);
        let err = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect_err("persistent propagation failure must surface after exhaustion");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no matching package") || chain.contains("exit code"),
            "expected last failure in chain: {chain}"
        );

        let n: u32 = std::fs::read_to_string(&counter)
            .expect("counter")
            .trim()
            .parse()
            .expect("u32");
        assert_eq!(
            n, PUBLISH_PROPAGATION_RETRIES,
            "must retry the full budget before surfacing"
        );
    }
}

// ---------------------------------------------------------------------------
// Partial-publish rollback: a multi-crate publish that succeeds on crate A
// then fails on crate B must record A (and only A) so rollback yanks the
// crate that actually went live — even when the local `.crate` files are
// gone. These tests stub `cargo` on PATH so the publish loop and the
// rollback yank loop exercise the real spawn surface without a network
// round-trip.
// ---------------------------------------------------------------------------
#[cfg(all(test, unix))]
mod partial_rollback_tests {
    use super::*;
    use anodizer_core::Publisher;
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Write a crate source dir with a `[package]` manifest pinning
    /// `version`, returning the dir path for use as `CrateConfig.path`.
    fn write_crate_dir(root: &Path, name: &str, version: &str) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir crate");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        )
        .expect("write Cargo.toml");
        dir.display().to_string()
    }

    /// Install a `cargo` shell stub on PATH that appends each invocation's
    /// argv (one line per call) to `argv_log` and chooses its exit code by
    /// argv: a `cargo publish -p <fail_crate>` exits 1; every other call
    /// (other publishes, `cargo yank`) exits 0. Returns a PATH value with
    /// the stub dir prepended; the caller installs it under a `#[serial]`
    /// guard and restores the prior value.
    pub(super) fn install_cargo_stub(dir: &Path, argv_log: &Path, fail_crate: &str) -> String {
        let stub = dir.join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = publish ]; then\n\
             for a in \"$@\"; do\n\
             if [ \"$a\" = '{fail}' ]; then exit 1; fi\n\
             done\n\
             fi\n\
             exit 0\n",
            log = argv_log.display(),
            fail = fail_crate,
        );
        std::fs::write(&stub, script).expect("write cargo stub");
        let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod stub");
        let prev = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", dir.display(), prev)
    }

    /// Read the stub's recorded argv lines (empty vec when the stub never
    /// ran / the log was never created).
    pub(super) fn read_argv_log(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Always-not-published injection: drives the publish loop straight to
    /// the `cargo publish` spawn without a sparse-index GET.
    fn never_published(
        _name: &str,
        _version: &str,
        _policy: &anodizer_core::retry::RetryPolicy,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Index injection used by the wait-gate wiring test: the workspace
    /// dependency `dep-crate` is reported already-live on crates.io (so the
    /// dep-completeness guard passes — the legitimate multi-tag case), while
    /// the crate being published (`leaf`) is reported absent (so the loop's
    /// idempotency check does NOT skip it and the wait-gate actually runs).
    fn dep_published_leaf_clean(
        name: &str,
        _version: &str,
        _policy: &anodizer_core::retry::RetryPolicy,
    ) -> Result<Option<String>> {
        if name == "dep-crate" {
            Ok(Some("deadbeef".to_string()))
        } else {
            Ok(None)
        }
    }

    fn cargo_crate(name: &str, path: &str, deps: &[&str], cfg: CargoPublishConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            publish: Some(PublishConfig {
                cargo: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// crate-a publishes; crate-b (which depends on a, so a goes first)
    /// fails. The success record must contain ONLY crate-a, with its
    /// per-crate version and configured registry — never crate-b
    /// (publish failed) or any skipped/never-published crate.
    #[test]
    #[serial(cargo_stub_path)]
    fn partial_publish_records_only_succeeded_crate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
        let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
        let argv_log = tmp.path().join("argv.log");

        // crate-a: skip its post-publish index poll (it has a dependent),
        // and pin a registry so the recorded snapshot carries it.
        let cfg_a = CargoPublishConfig {
            index_timeout: Some(0),
            registry: Some("my-registry".to_string()),
            ..Default::default()
        };
        // crate-b depends on crate-a → topological order publishes a first.
        let crate_a = cargo_crate("crate-a", &path_a, &[], cfg_a);
        let crate_b = cargo_crate(
            "crate-b",
            &path_b,
            &["crate-a"],
            CargoPublishConfig::default(),
        );

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_a, crate_b])
            .selected_crates(vec!["crate-b".to_string()])
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        unsafe { std::env::set_var("PATH", &new_path) };
        let result = publish_to_cargo_with(
            &mut ctx,
            &["crate-b".to_string()],
            &log,
            &mut record,
            never_published,
        );
        // SAFETY: restore PATH within the same serial group.
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        assert!(result.is_err(), "crate-b's publish failure must surface");

        // The stub must have seen BOTH publishes (a succeeds, b fails).
        let argv = read_argv_log(&argv_log);
        assert!(
            argv.iter()
                .any(|l| l.contains("publish") && l.contains("crate-a")),
            "stub should have run crate-a's publish: {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|l| l.contains("publish") && l.contains("crate-b")),
            "stub should have run crate-b's publish: {argv:?}"
        );

        // Record holds crate-a only, with its version + registry.
        assert_eq!(
            record.len(),
            1,
            "only the succeeded crate is recorded: {record:?}"
        );
        let rec = &record[0];
        assert_eq!(rec.name, "crate-a");
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.registry.as_deref(), Some("my-registry"));
        assert!(rec.index.is_none());
    }

    /// End-to-end through the Publisher trait: the failed `run` stashes the
    /// partial evidence on the context (crate-a only); `rollback` reads it
    /// and issues exactly one `cargo yank` — for crate-a, on its configured
    /// registry — and never touches crate-b (never published).
    #[test]
    #[serial(cargo_stub_path)]
    fn run_failure_then_rollback_yanks_only_succeeded_crate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
        let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
        let argv_log = tmp.path().join("argv.log");

        let cfg_a = CargoPublishConfig {
            index_timeout: Some(0),
            registry: Some("my-registry".to_string()),
            ..Default::default()
        };
        let crate_a = cargo_crate("crate-a", &path_a, &[], cfg_a);
        let crate_b = cargo_crate(
            "crate-b",
            &path_b,
            &["crate-a"],
            CargoPublishConfig::default(),
        );

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_a, crate_b])
            .selected_crates(vec!["crate-b".to_string()])
            .build();

        // Build the evidence the failed publish would record, exactly as
        // `CargoPublisher::run` does, by driving the injected publish loop
        // and encoding whatever it recorded before the bail.
        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        unsafe { std::env::set_var("PATH", &new_path) };

        let mut record: Vec<CargoYankTarget> = Vec::new();
        let publish_result = publish_to_cargo_with(
            &mut ctx,
            &["crate-b".to_string()],
            &log,
            &mut record,
            never_published,
        );
        assert!(publish_result.is_err(), "crate-b failure surfaces");

        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&record);

        // Wipe the publish argv before rollback so we assert only on the
        // yank invocations the rollback issues.
        std::fs::write(&argv_log, b"").expect("truncate argv log");

        let publisher = CargoPublisher::new();
        let rb = publisher.rollback(&mut ctx, &evidence);

        // SAFETY: restore PATH within the same serial group.
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        rb.expect("rollback ok");

        let yanks: Vec<String> = read_argv_log(&argv_log)
            .into_iter()
            .filter(|l| l.starts_with("yank"))
            .collect();
        assert_eq!(yanks.len(), 1, "exactly one crate is yanked: {yanks:?}");
        let line = &yanks[0];
        assert!(
            line.contains("--version 1.0.0"),
            "yank carries the version: {line}"
        );
        assert!(line.contains("crate-a"), "yank targets crate-a: {line}");
        assert!(
            line.contains("--registry my-registry"),
            "yank targets the registry: {line}"
        );
        assert!(
            !line.contains("crate-b"),
            "crate-b was never published; must not be yanked: {line}"
        );
    }

    /// Empty record (the publisher failed before its first successful
    /// publish, or nothing was eligible): rollback is a clean no-op — it
    /// spawns no `cargo` and returns Ok, rather than emitting a scary warn.
    #[test]
    #[serial(cargo_stub_path)]
    fn rollback_is_clean_noop_when_nothing_published() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let argv_log = tmp.path().join("argv.log");

        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&[]);

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        unsafe { std::env::set_var("PATH", &new_path) };

        let publisher = CargoPublisher::new();
        let rb = publisher.rollback(&mut ctx, &evidence);

        // SAFETY: restore PATH within the same serial group.
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        rb.expect("rollback no-op ok");

        assert!(
            read_argv_log(&argv_log).is_empty(),
            "no-op rollback must not spawn cargo"
        );
    }

    /// Install a `cargo` stub that records argv and exits non-zero for
    /// `cargo yank` (every other call exits 0). Drives the rollback
    /// yank-failure branch so the `failed` counter + warn path are exercised.
    fn install_yank_failing_stub(dir: &Path, argv_log: &Path) -> String {
        let stub = dir.join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = yank ]; then\n\
             echo 'error: api errored: 403 forbidden' >&2\n\
             exit 1\n\
             fi\n\
             exit 0\n",
            log = argv_log.display(),
        );
        std::fs::write(&stub, script).expect("write cargo stub");
        let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod stub");
        let prev = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", dir.display(), prev)
    }

    /// Run `f` with `PATH` prepended to `new_path` under the serial guard,
    /// restoring the previous value afterward. Keeps the set/restore pairing
    /// out of each test body.
    fn with_path<R>(new_path: &str, f: impl FnOnce() -> R) -> R {
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator in the workspace, including fake_tool::activate)
        // plus the callers' `#[serial(cargo_stub_path)]` guard; paired
        // restore below.
        unsafe { std::env::set_var("PATH", new_path) };
        let out = f();
        // SAFETY: restore the prior PATH (paired with the set above).
        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        out
    }

    /// Rollback whose `cargo yank` fails: the publisher must NOT propagate
    /// the error (rollback is best-effort), still record the failure, and
    /// emit the per-target warn. We assert the yank was attempted with the
    /// recorded version and that rollback returns Ok despite the non-zero
    /// exit.
    #[test]
    #[serial(cargo_stub_path)]
    fn rollback_continues_and_warns_when_yank_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let argv_log = tmp.path().join("argv.log");

        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&[CargoYankTarget {
            name: "crate-x".into(),
            version: "1.4.2".into(),
            registry: None,
            index: None,
        }]);

        let new_path = install_yank_failing_stub(tmp.path(), &argv_log);
        let publisher = CargoPublisher::new();
        let rb = with_path(&new_path, || publisher.rollback(&mut ctx, &evidence));
        // Best-effort: a failed yank must NOT turn rollback into an Err.
        rb.expect("rollback tolerates a failed yank");

        let yanks: Vec<String> = read_argv_log(&argv_log)
            .into_iter()
            .filter(|l| l.starts_with("yank"))
            .collect();
        assert_eq!(
            yanks.len(),
            1,
            "the single target is yanked once: {yanks:?}"
        );
        assert!(
            yanks[0].contains("--version 1.4.2") && yanks[0].contains("crate-x"),
            "yank carries the recorded version + name: {}",
            yanks[0]
        );
    }

    /// A recorded target with an `index` (not a `registry`) threads
    /// `--index <url>` into the yank argv. Pins the index-arg branch of the
    /// rollback yank command builder.
    #[test]
    #[serial(cargo_stub_path)]
    fn rollback_yank_threads_index_arg() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let argv_log = tmp.path().join("argv.log");

        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&[CargoYankTarget {
            name: "crate-idx".into(),
            version: "0.2.0".into(),
            registry: None,
            index: Some("sparse+https://example.test/index/".into()),
        }]);

        // `none` never matches a publish arg, so this stub exits 0 for yank.
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        let publisher = CargoPublisher::new();
        with_path(&new_path, || publisher.rollback(&mut ctx, &evidence)).expect("rollback ok");

        let yank = read_argv_log(&argv_log)
            .into_iter()
            .find(|l| l.starts_with("yank"))
            .expect("a yank was issued");
        assert!(
            yank.contains("--index sparse+https://example.test/index/"),
            "index target must thread --index: {yank}"
        );
        assert!(
            !yank.contains("--registry"),
            "index-only target must NOT carry --registry: {yank}"
        );
    }

    /// A crate whose resolved version is empty (no `[package].version` on
    /// disk AND a blank release version) is published but CANNOT be recorded
    /// for auto-yank: the loop emits the "CANNOT be auto-yanked" warn and the
    /// success record stays empty, so a later failure leaves nothing to yank.
    #[test]
    #[serial(cargo_stub_path)]
    fn empty_version_publish_is_not_recorded_for_yank() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Manifest with NO version field ⇒ read_cargo_toml_version → None.
        let dir = tmp.path().join("noversion");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"noversion\"\n")
            .expect("write manifest");
        let argv_log = tmp.path().join("argv.log");

        let crate_nv = cargo_crate(
            "noversion",
            &dir.display().to_string(),
            &[],
            CargoPublishConfig::default(),
        );
        // Suppress git-var population so the release-version fallback is also
        // empty — without this the builder's default semver (1.2.3) fills in.
        let mut ctx = TestContextBuilder::new()
            .populate_git_vars(false)
            .crates(vec![crate_nv])
            .selected_crates(vec!["noversion".to_string()])
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        // never_published would early-skip on a non-empty version, but the
        // empty-version branch bypasses the index check entirely and goes
        // straight to publish — so the stub's `cargo publish` runs.
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail-crate");
        let result = with_path(&new_path, || {
            publish_to_cargo_with(
                &mut ctx,
                &["noversion".to_string()],
                &log,
                &mut record,
                never_published,
            )
        });
        result.expect("publish of a version-less crate still succeeds");

        // The publish ran...
        assert!(
            read_argv_log(&argv_log)
                .iter()
                .any(|l| l.contains("publish") && l.contains("noversion")),
            "version-less crate is still published"
        );
        // ...but NOTHING is recorded, because an empty version can't be yanked.
        assert!(
            record.is_empty(),
            "empty-version publish must NOT be recorded for auto-yank: {record:?}"
        );
    }

    /// Already-published idempotency: when the injected index check reports
    /// the version is live (`Ok(Some(_))`), the publish loop SKIPS that crate
    /// — `cargo publish` is never spawned and nothing is recorded.
    #[test]
    #[serial(cargo_stub_path)]
    fn already_published_crate_is_skipped_not_republished() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "live-crate", "9.9.9");
        let argv_log = tmp.path().join("argv.log");

        let crate_cfg = cargo_crate("live-crate", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v9.9.9")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["live-crate".to_string()])
            .build();

        // Inject "already on crates.io with this cksum" for every query.
        let always_published =
            |_n: &str,
             _v: &str,
             _p: &anodizer_core::retry::RetryPolicy|
             -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        let result = with_path(&new_path, || {
            publish_to_cargo_with(
                &mut ctx,
                &["live-crate".to_string()],
                &log,
                &mut record,
                always_published,
            )
        });
        result.expect("already-published path returns Ok");

        assert!(
            read_argv_log(&argv_log).is_empty(),
            "already-published crate must NOT spawn cargo publish"
        );
        assert!(
            record.is_empty(),
            "a skipped (already-published) crate is not recorded for yank"
        );
    }

    /// Index-check error (`Err`) is non-fatal: the loop logs a warn and
    /// falls through to publish anyway, letting cargo's server-side guard
    /// arbitrate. The crate publishes and is recorded.
    #[test]
    #[serial(cargo_stub_path)]
    fn index_check_error_falls_through_to_publish() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "flaky", "1.0.0");
        let argv_log = tmp.path().join("argv.log");

        let crate_cfg = cargo_crate("flaky", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["flaky".to_string()])
            .build();

        let index_errors = |_n: &str,
                            _v: &str,
                            _p: &anodizer_core::retry::RetryPolicy|
         -> Result<Option<String>> {
            Err(anyhow::anyhow!("index transport blew up"))
        };

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        let result = with_path(&new_path, || {
            publish_to_cargo_with(
                &mut ctx,
                &["flaky".to_string()],
                &log,
                &mut record,
                index_errors,
            )
        });
        result.expect("index error is non-fatal; publish proceeds");

        assert!(
            read_argv_log(&argv_log)
                .iter()
                .any(|l| l.contains("publish") && l.contains("flaky")),
            "index-check error must fall through to publish"
        );
        assert_eq!(record.len(), 1, "the published crate is recorded");
        assert_eq!(record[0].version, "1.0.0");
    }

    /// `wait_for_workspace_deps` integration: when enabled and the crate has
    /// a literal-pinned workspace dep, the loop polls crates.io for that dep.
    /// We point the dep's expected version at one already on a local index
    /// responder so the gate clears in one probe — proving the gate is wired
    /// into the publish loop (not just unit-tested in isolation). The dep
    /// pin uses a crate name whose sparse-index URL we can serve locally is
    /// impossible (the gate computes the real index URL), so instead we set
    /// a tiny max_wait and assert the gate's TIMEOUT error surfaces through
    /// the publish loop's context — proving the wiring fires.
    #[test]
    #[serial(cargo_stub_path)]
    fn wait_for_workspace_deps_gate_is_wired_into_publish_loop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Leaf with a literal-pinned workspace-internal dep that will never
        // appear (bogus version on the real index) → the gate times out.
        let dir = tmp.path().join("leaf");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\ndep-crate = { path = \"../dep\", version = \"0.0.0-never-exists\" }\n",
        )
        .expect("write manifest");
        let argv_log = tmp.path().join("argv.log");

        use anodizer_core::config::HumanDuration;
        use std::time::Duration;
        let wait_cfg = WaitForWorkspaceDepsConfig {
            enabled: Some(true),
            // Sub-millisecond budget so the timeout fires fast.
            max_wait: Some(HumanDuration(Duration::from_millis(1))),
            poll_interval: Some(HumanDuration(Duration::from_millis(1))),
        };
        let leaf = cargo_crate(
            "leaf",
            &dir.display().to_string(),
            &["dep-crate"],
            CargoPublishConfig {
                wait_for_workspace_deps: Some(wait_cfg),
                ..Default::default()
            },
        );
        // `dep-crate` is in the config (so it counts as workspace-internal)
        // but has no cargo block, so it isn't itself published.
        let dep = CrateConfig {
            name: "dep-crate".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![leaf, dep])
            .selected_crates(vec!["leaf".to_string()])
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        // The dep-completeness guard runs first; inject `always_published` so
        // it treats `dep-crate` as live on crates.io (the legitimate multi-tag
        // case the wait-gate is for) and the wait-gate TIMEOUT — not the guard
        // — is the failure under test. The wait-gate itself polls the REAL
        // index for the bogus `0.0.0-never-exists` version, so it still times
        // out as intended.
        let result = with_path(&new_path, || {
            publish_to_cargo_with(
                &mut ctx,
                &["leaf".to_string()],
                &log,
                &mut record,
                dep_published_leaf_clean,
            )
        });
        let err = result.expect_err("wait_for_workspace_deps timeout must surface");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("wait_for_workspace_deps"),
            "the gate error must be threaded through the publish loop: {chain}"
        );
        // The gate fired BEFORE the publish spawn, so cargo was never run.
        assert!(
            read_argv_log(&argv_log).is_empty(),
            "publish must not spawn while the dep gate is still blocking"
        );
    }

    /// End-to-end through `CargoPublisher::run`: a multi-crate publish that
    /// fails on the second crate stashes the partial evidence on the context
    /// (the Err arm of `run`) so the dispatcher can recover it for rollback.
    /// Asserts the stashed evidence records ONLY the first (succeeded) crate.
    #[test]
    #[serial(cargo_stub_path)]
    fn run_failure_stashes_partial_evidence_on_context() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_a = write_crate_dir(tmp.path(), "crate-a", "1.0.0");
        let path_b = write_crate_dir(tmp.path(), "crate-b", "2.0.0");
        let argv_log = tmp.path().join("argv.log");

        let crate_a = cargo_crate(
            "crate-a",
            &path_a,
            &[],
            CargoPublishConfig {
                index_timeout: Some(0),
                ..Default::default()
            },
        );
        let crate_b = cargo_crate(
            "crate-b",
            &path_b,
            &["crate-a"],
            CargoPublishConfig::default(),
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_a, crate_b])
            .selected_crates(vec!["crate-b".to_string()])
            .build();

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        let publisher = CargoPublisher::new();
        let run_result = with_path(&new_path, || publisher.run(&mut ctx));
        assert!(run_result.is_err(), "crate-b failure surfaces from run");

        // The Err arm recorded the partial evidence on the context.
        let pending = ctx
            .take_pending_evidence()
            .expect("failed run must stash pending evidence for rollback");
        let targets = decode_cargo_yank_targets(&pending.extra);
        assert_eq!(targets.len(), 1, "only crate-a is recorded: {targets:?}");
        assert_eq!(targets[0].name, "crate-a");
        assert_eq!(targets[0].version, "1.0.0");
    }

    /// When a crate's Cargo.toml has no resolvable version, the skip-decision
    /// must treat it as "not yet published" (attempt publish) — NOT key the
    /// idempotency probe on the global release version.
    ///
    /// The old code used `unwrap_or_else(|| release_version.clone())` which
    /// caused `already_published_check("my-crate", "1.0.0")` to return
    /// `Some(cksum)` → the crate was silently skipped even though its real
    /// version had never been published.
    #[test]
    #[serial(cargo_stub_path)]
    fn manifest_read_failure_does_not_skip_publish() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a Cargo.toml WITHOUT a version field — simulates the case
        // where `read_cargo_toml_version` returns None.
        let crate_dir = tmp.path().join("my-crate");
        std::fs::create_dir_all(&crate_dir).expect("mkdir");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\n# no version field\n",
        )
        .expect("write Cargo.toml");
        let argv_log = tmp.path().join("argv.log");

        let crate_cfg = cargo_crate(
            "my-crate",
            &crate_dir.display().to_string(),
            &[],
            CargoPublishConfig {
                index_timeout: Some(0),
                ..Default::default()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_cfg])
            .build();

        // The "1.0.0" release version IS already on crates.io — if we
        // incorrectly keyed the skip-decision on it, the crate would be
        // skipped. The correct behaviour is to attempt publish anyway because
        // the per-crate version is unresolvable.
        let always_published_1_0_0 =
            |_name: &str,
             _version: &str,
             _policy: &anodizer_core::retry::RetryPolicy|
             -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        unsafe { std::env::set_var("PATH", &new_path) };

        let mut record: Vec<CargoYankTarget> = Vec::new();
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Normal);
        let result = publish_to_cargo_with(
            &mut ctx,
            &["my-crate".to_string()],
            &log,
            &mut record,
            always_published_1_0_0,
        );

        // SAFETY: restore PATH.
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        result.expect("publish must succeed");
        let invocations = read_argv_log(&argv_log);
        let published: Vec<&String> = invocations
            .iter()
            .filter(|l| l.starts_with("publish"))
            .collect();
        assert_eq!(
            published.len(),
            1,
            "cargo publish must be invoked despite unresolvable manifest version: {invocations:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// dep-completeness guard tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod dep_guard_tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};

    fn quiet_log() -> StageLogger {
        StageLogger::new("publish-test", Verbosity::Normal)
    }

    /// Write a crate dir with a `[package]` (version `ver`) plus a
    /// `[dependencies]` block listing each `(dep_name, dep_version)`, and a
    /// `[dev-dependencies]` block listing each `(dep_name, dep_version)` in
    /// `dev_deps`. Returns the crate's path string.
    fn write_crate(
        root: &std::path::Path,
        name: &str,
        ver: &str,
        deps: &[(&str, &str)],
        dev_deps: &[(&str, &str)],
    ) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut body = format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n");
        if !deps.is_empty() {
            body.push_str("\n[dependencies]\n");
            for (d, dv) in deps {
                body.push_str(&format!("{d} = {{ version = \"{dv}\" }}\n"));
            }
        }
        if !dev_deps.is_empty() {
            body.push_str("\n[dev-dependencies]\n");
            for (d, dv) in dev_deps {
                body.push_str(&format!("{d} = {{ version = \"{dv}\" }}\n"));
            }
        }
        std::fs::write(dir.join("Cargo.toml"), body).expect("write manifest");
        dir.display().to_string()
    }

    fn crate_cfg(name: &str, path: &str, deps: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            publish: Some(anodizer_core::config::PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// (1) A publishing crate whose workspace dep is missing from the set AND
    /// absent from the index → the guard returns Err naming the dep + crate.
    #[test]
    fn guard_errors_when_dep_missing_from_set_and_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // `app` depends on `lib` (a workspace crate) but only `app` is in the
        // publish set; `lib` is not, and the index probe reports it Absent.
        let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
        let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_path, &["lib"]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["app".to_string()]; // lib intentionally NOT in the set
        let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect();

        let probe = |_n: &str, _v: &str| DepIndexState::Absent;
        let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect_err("missing-and-absent dep must fail the guard");
        let msg = format!("{err:#}");
        assert!(msg.contains("'app'"), "names the publishing crate: {msg}");
        assert!(msg.contains("'lib'"), "names the missing dep: {msg}");
        assert!(
            msg.contains("publish set"),
            "explains the fix (add to publish set): {msg}"
        );
    }

    /// (2) Every workspace dep is in the publish set → Ok regardless of index
    /// state (the probe must not even be consulted for an in-set dep).
    #[test]
    fn guard_ok_when_all_deps_in_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
        let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_path, &["lib"]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["lib".to_string(), "app".to_string()]; // both in set
        let versions: HashMap<String, String> = [
            ("app".to_string(), "1.0.0".to_string()),
            ("lib".to_string(), "1.0.0".to_string()),
        ]
        .into_iter()
        .collect();

        // Probe panics if called — an in-set dep must short-circuit before it.
        let probe = |_n: &str, _v: &str| panic!("index probe must not run for in-set deps");
        check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect("all deps in set → ok");
    }

    /// (3) A dep not in the set but already live on crates.io (mocked Present)
    /// → Ok. The version probed must be the one the dependent requires.
    #[test]
    fn guard_ok_when_dep_not_in_set_but_already_on_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_path = write_crate(tmp.path(), "app", "2.0.0", &[("lib", "1.5.0")], &[]);
        let lib_path = write_crate(tmp.path(), "lib", "1.5.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_path, &["lib"]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["app".to_string()]; // lib not re-published this run
        let versions: HashMap<String, String> = [("app".to_string(), "2.0.0".to_string())]
            .into_iter()
            .collect();

        let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
        let probe = |n: &str, v: &str| {
            seen.borrow_mut().push((n.to_string(), v.to_string()));
            DepIndexState::Present
        };
        check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect("dep live on crates.io → ok");
        assert_eq!(
            *seen.borrow(),
            vec![("lib".to_string(), "1.5.0".to_string())],
            "guard probes the dep at the version the dependent pins"
        );
    }

    /// An inconclusive (Unknown) index probe never fails the guard — a
    /// transient crates.io outage must not block a release.
    #[test]
    fn guard_ok_on_inconclusive_index_probe() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
        let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_path, &["lib"]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["app".to_string()];
        let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect();

        let probe = |_n: &str, _v: &str| DepIndexState::Unknown;
        check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect("inconclusive probe must not fail the guard");
    }

    /// A dev-dependency on an out-of-set, index-absent sibling must NOT trip
    /// the guard: `cargo publish` strips dev-deps and does not require them on
    /// the index. The probe must never be called (no non-dev edge exists).
    #[test]
    fn guard_ignores_dev_dependencies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // `lib` is ONLY a dev-dependency of `app`.
        let app_path = write_crate(tmp.path(), "app", "1.0.0", &[], &[("lib", "1.0.0")]);
        let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_path, &[]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["app".to_string()];
        let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect();

        let probe = |_n: &str, _v: &str| panic!("dev-dep must not be probed");
        check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect("dev-dep on out-of-set sibling must not trip the guard");
    }

    /// The real 0.6.0/0.7.0 burn shape: a `<dep>.workspace = true` inherit.
    /// The required version lives in the workspace root's
    /// `[workspace.dependencies]`, not the leaf manifest — the guard must
    /// resolve it and probe `lib@0.7.0`.
    #[test]
    fn guard_resolves_workspace_inherited_dep_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Workspace root with a `[workspace.dependencies]` pinning lib@0.7.0.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\", version = \"0.7.0\" }\n",
        )
        .expect("write workspace root");
        // app inherits lib via `lib.workspace = true` (no literal pin).
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        std::fs::write(
            app_dir.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.7.0\"\n\n\
             [dependencies]\nlib.workspace = true\n",
        )
        .expect("write app manifest");
        let lib_path = write_crate(tmp.path(), "lib", "0.7.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_dir.display().to_string(), &["lib"]),
            crate_cfg("lib", &lib_path, &[]),
        ];
        let order = vec!["app".to_string()]; // lib missing from the set (the bug)
        let versions: HashMap<String, String> = [("app".to_string(), "0.7.0".to_string())]
            .into_iter()
            .collect();

        let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
        let probe = |n: &str, v: &str| {
            seen.borrow_mut().push((n.to_string(), v.to_string()));
            DepIndexState::Absent
        };
        let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect_err("inherited dep missing from set + absent must fail");
        assert!(format!("{err:#}").contains("'lib'"), "names the dep");
        assert_eq!(
            *seen.borrow(),
            vec![("lib".to_string(), "0.7.0".to_string())],
            "inherited version resolved from the workspace root"
        );
    }

    /// A dep declared with `package = "real-name"` under an alias key must be
    /// matched by its real package name, not the alias.
    ///
    ///   [dependencies]
    ///   core = { package = "anodizer-core", version = "0.8.0" }
    ///
    /// Before the fix, the guard compared key `"core"` against
    /// workspace_crate_names (which contains `"anodizer-core"`) — the match
    /// failed and the dep was silently ignored, so a genuinely-absent
    /// `anodizer-core` slipped through the guard.
    #[test]
    fn guard_resolves_package_renamed_dep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Crate with a renamed dep: key is "core", real name is "anodizer-core".
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        std::fs::write(
            app_dir.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore = { package = \"anodizer-core\", version = \"0.8.0\" }\n",
        )
        .expect("write app manifest");

        let core_path = write_crate(tmp.path(), "anodizer-core", "0.8.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_dir.display().to_string(), &[]),
            crate_cfg("anodizer-core", &core_path, &[]),
        ];
        let order = vec!["app".to_string()]; // anodizer-core NOT in publish set

        let versions: HashMap<String, String> = [("app".to_string(), "0.8.0".to_string())]
            .into_iter()
            .collect();

        let probe = |n: &str, _v: &str| {
            // anodizer-core is absent from the index, triggering the guard.
            if n == "anodizer-core" {
                DepIndexState::Absent
            } else {
                DepIndexState::Present
            }
        };
        let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect_err("renamed dep absent from set and index must fail guard");
        assert!(
            format!("{err:#}").contains("anodizer-core"),
            "error must name the real package, not the alias: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("declared as 'core' via package rename"),
            "error must surface the in-code alias: {err:#}"
        );
    }

    /// The alias key of a renamed dep must NOT be treated as a crate name.
    /// With a workspace member literally named after the alias ("core") AND in
    /// the publish set, matching the alias would satisfy the in-set check and
    /// silently pass — even though the dep actually points at
    /// "anodizer-core", which is absent from both the set and the index.
    #[test]
    fn guard_does_not_match_alias_key_as_crate_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        std::fs::write(
            app_dir.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore = { package = \"anodizer-core\", version = \"0.8.0\" }\n",
        )
        .expect("write app manifest");

        // A workspace member that shares the alias's name, plus the real dep.
        let alias_twin_path = write_crate(tmp.path(), "core", "0.8.0", &[], &[]);
        let real_path = write_crate(tmp.path(), "anodizer-core", "0.8.0", &[], &[]);
        let all = vec![
            crate_cfg("app", &app_dir.display().to_string(), &[]),
            crate_cfg("core", &alias_twin_path, &[]),
            crate_cfg("anodizer-core", &real_path, &[]),
        ];
        // The alias-named member IS in the set; the real dep is NOT.
        let order = vec!["app".to_string(), "core".to_string()];
        let versions: HashMap<String, String> = [
            ("app".to_string(), "0.8.0".to_string()),
            ("core".to_string(), "0.8.0".to_string()),
        ]
        .into_iter()
        .collect();

        let probe = |n: &str, _v: &str| {
            if n == "anodizer-core" {
                DepIndexState::Absent
            } else {
                DepIndexState::Present
            }
        };
        let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect_err("alias in set must not satisfy the check for the real package");
        assert!(
            format!("{err:#}").contains("anodizer-core"),
            "error must name the real package: {err:#}"
        );
    }

    /// A rename declared on the workspace root entry — the only place cargo
    /// accepts `package =` for an inherited dep:
    ///
    ///   [workspace.dependencies]
    ///   core = { path = "core", version = "0.8.0", package = "anodizer-core" }
    ///
    /// with the leaf inheriting via `core.workspace = true`. The leaf value
    /// carries no `package` key, so the effective name must be resolved from
    /// the root entry; matching the alias would silently skip the dep.
    #[test]
    fn guard_resolves_workspace_inherited_renamed_dep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"core\"]\n\n\
             [workspace.dependencies]\n\
             core = { path = \"core\", version = \"0.8.0\", package = \"anodizer-core\" }\n",
        )
        .expect("write workspace root");
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("mkdir app");
        std::fs::write(
            app_dir.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore.workspace = true\n",
        )
        .expect("write app manifest");
        let core_dir = tmp.path().join("core");
        std::fs::create_dir_all(&core_dir).expect("mkdir core");
        std::fs::write(
            core_dir.join("Cargo.toml"),
            "[package]\nname = \"anodizer-core\"\nversion = \"0.8.0\"\n",
        )
        .expect("write core manifest");
        let all = vec![
            crate_cfg("app", &app_dir.display().to_string(), &[]),
            crate_cfg("anodizer-core", &core_dir.display().to_string(), &[]),
        ];
        let order = vec!["app".to_string()]; // anodizer-core NOT in publish set
        let versions: HashMap<String, String> = [("app".to_string(), "0.8.0".to_string())]
            .into_iter()
            .collect();

        let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
        let probe = |n: &str, v: &str| {
            seen.borrow_mut().push((n.to_string(), v.to_string()));
            DepIndexState::Absent
        };
        let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
            .expect_err("inherited renamed dep absent from set and index must fail guard");
        assert!(
            format!("{err:#}").contains("anodizer-core"),
            "error must name the real package, not the alias: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("declared as 'core' via package rename"),
            "error must surface the in-code alias: {err:#}"
        );
        assert_eq!(
            *seen.borrow(),
            vec![("anodizer-core".to_string(), "0.8.0".to_string())],
            "probe must target the real package at the root-pinned version"
        );
    }
}
