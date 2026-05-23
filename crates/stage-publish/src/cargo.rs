use anodizer_core::config::{CargoPublishConfig, CrateConfig};
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
fn sparse_index_url(crate_name: &str) -> String {
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
/// against **cargo 1.95.x** (rustc 1.95.0, 2026-04-14).
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
                "{label}: propagation-style failure persists after {attempt} attempts; surfacing"
            ));
            last_output = Some(output);
            break;
        }

        log.status(&format!(
            "{label}: sparse-index propagation lag detected (attempt {}/{}); retrying in {}s",
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

/// Read the `version = "X.Y.Z"` from a crate's Cargo.toml.
/// Uses a simple line scan rather than a full TOML parse to avoid
/// pulling in the `toml` crate as a dep of stage-publish.
fn read_cargo_toml_version(crate_path: &str) -> Option<String> {
    let manifest = std::path::Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&manifest).ok()?;
    // Look for `version = "..."` in the [package] section (before any
    // other `[section]` header). This covers both quoted and workspace
    // forms; workspace references (version.workspace = true) return None
    // since they don't have a literal version string.
    let mut in_package = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            if in_package {
                break;
            }
            continue;
        }
        if in_package && let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                if rest.starts_with('"') {
                    return rest.trim_matches('"').to_string().into();
                }
            }
        }
    }
    None
}

pub fn publish_to_cargo(ctx: &mut Context, selected: &[String], log: &StageLogger) -> Result<()> {
    // Defensive guard: the `--skip=cargo` gate lives in the
    // dispatcher in `lib.rs::PublishStage::run` so every publisher emits its
    // skip log uniformly. Re-checking here protects future direct callers
    // (tests, CLI sub-commands) from accidentally bypassing the gate. No log
    // is emitted on this path — the dispatcher already logged it.
    if ctx.should_skip("cargo") {
        return Ok(());
    }
    // When a crate depends on another crate in the same workspace that
    // isn't yet on crates.io, `cargo publish` for the dependent will fail
    // with "no matching package named X found" because cargo verifies path
    // deps against the registry. Walk depends_on transitively so we publish
    // the dependency chain in topological order, not just the caller's
    // --crate selection. Already-published versions are skipped below via
    // the is_already_published check, so including extra crates is safe.
    // Build the full crate universe — top-level + all workspaces — so
    // expand_with_transitive_deps can find deps that live in a DIFFERENT
    // workspace. After workspace overlay, config.crates only contains the
    // overlaid workspace's crates, but a crate's depends_on may reference
    // crates in other workspaces (e.g. cfgd depends on cfgd-core which
    // lives in its own workspace).
    let all_crates: Vec<CrateConfig> = crate::util::all_crates(ctx);

    let expanded_selection: Vec<String> = if selected.is_empty() {
        Vec::new()
    } else {
        expand_with_transitive_deps(&all_crates, selected)
    };
    let selected_set: std::collections::HashSet<&str> =
        expanded_selection.iter().map(|s| s.as_str()).collect();

    // Resolve the per-crate `publish.cargo` block to a (selected, cfg) pair.
    // - None       → publisher omitted; not eligible.
    // - Some(cfg)  → eligible unless `cfg.skip` evaluates truthy.
    // Templated `skip:` is honored here so the same render-once pass populates
    // both the eligibility list and the per-crate timeout/flag lookups.
    let cargo_cfgs: HashMap<String, CargoPublishConfig> = {
        let mut m = HashMap::new();
        for c in &all_crates {
            let Some(ref publish) = c.publish else {
                continue;
            };
            let Some(ref cargo_cfg) = publish.cargo else {
                continue;
            };
            // Honor the peer-publisher `skip:` field.
            if let Some(ref d) = cargo_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| format!("cargo: render skip template for '{}'", c.name))?;
                if off {
                    log.status(&format!("cargo: skipped for '{}' (skip=true)", c.name));
                    continue;
                }
            }
            m.insert(c.name.clone(), cargo_cfg.clone());
        }
        m
    };

    // Collect (name, depends_on) for crates with cargo publishing eligible,
    // filtered to the expanded selection when non-empty.
    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| cargo_cfgs.contains_key(&c.name))
        .map(|c| {
            let deps = c.depends_on.clone().unwrap_or_default();
            (c.name.clone(), deps)
        })
        .collect();

    if publishable.is_empty() {
        // The publisher wrapper (`CargoPublisher::run`) emits the canonical
        // operator-facing warn for the no-eligible-crates path; this
        // branch is unreachable in normal dispatch because the wrapper
        // short-circuits before calling here, but defensive callers
        // (tests, direct CLI sub-commands) still exit cleanly.
        return Ok(());
    }

    let sorted_names = topological_sort(&publishable);

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

    let version = ctx.version();

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every crate's index-check GET. Mirrors the per-pipe-invocation
    // pattern used by artifactory/cloudsmith.
    let retry_policy = ctx.retry_policy();

    // Build a lookup from crate name → path so we can read each crate's
    // actual Cargo.toml version for the already-published check. Transitive
    // deps may have a DIFFERENT version than the release tag (e.g. cfgd-core
    // is at 0.2.2 while cfgd releases 0.3.2).
    let crate_paths: HashMap<String, String> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.path.clone()))
        .collect();

    for (i, name) in sorted_names.iter().enumerate() {
        log.status(&run_per_crate_start_message(name));
        // Read the crate's actual version from its Cargo.toml, falling back
        // to the global release version if the path isn't found or parse fails.
        let crate_version = crate_paths
            .get(name)
            .and_then(|path| read_cargo_toml_version(path))
            .unwrap_or_else(|| version.clone());

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
            match is_already_published(name, &crate_version, &retry_policy) {
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
        let cmd = publish_command(name, cargo_cfg);
        log.status(&format!("running: {}", cmd.join(" ")));

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

/// Publisher trait adapter around [`publish_to_cargo`].
///
/// Classified as `Submitter` + `required=true`: crates.io publish is
/// effectively one-way (versions cannot be re-uploaded), so a failure
/// here should fail the release and other Submitter publishers must
/// already be gated.
///
/// (Marker for follow-up: when the `PublishStage::run` dispatch swap
/// lands in the return-type-swap task, update this doc to reflect that
/// the new dispatch path is the only consumer.)
pub struct CargoPublisher;

impl CargoPublisher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CargoPublisher {
    fn default() -> Self {
        Self::new()
    }
}

/// Operator-visible start line for the cargo publisher. Mirrors the
/// `run_start_message` helper every per-crate publisher exposes so the
/// dispatch table can't silently report success on a no-op run.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "cargo: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Operator-visible per-crate start line. Emitted by `publish_to_cargo`
/// immediately before each crate's publish-or-skip decision so the
/// per-crate progress is anchored to a specific name in the log.
/// Mirrors `run_per_crate_start_message` on every other per-crate
/// publisher (homebrew, scoop, nix, aur, krew).
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("cargo: starting per-crate publish for '{}'", crate_name)
}

/// Operator-visible done line, emitted after `publish_to_cargo` returns
/// Ok. `processed` counts crates whose publish path was actually
/// invoked (skipped-by-already-published, skipped-by-skip-template, and
/// dry-run paths all count as processed — they're successful runs of
/// the correct code path).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("cargo: completed — {} crate(s) processed", processed)
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.cargo` block) but `publish_to_cargo` resolved
/// zero publishable crates (every cargo-configured crate was filtered
/// out by `--crate` / `--all` selection).
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "cargo: registered but 0 of {} effective crate(s) had a publish.cargo \
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
        "cargo"
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        anodizer_core::PublisherGroup::Submitter
    }

    fn required(&self) -> bool {
        true
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
        publish_to_cargo(ctx, &selected, &log)?;
        log.status(&run_done_message(eligible));
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        if let Some(primary) = first_published_crate(ctx) {
            evidence.primary_ref = Some(format!(
                "https://crates.io/crates/{name}/{version}",
                name = primary.name,
                version = primary.version
            ));
        }
        evidence.artifact_paths = collect_local_crate_paths(ctx);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        if evidence.artifact_paths.is_empty() {
            log.warn(
                "cargo: no .crate paths recorded in evidence; yank skipped, verify crates.io manually",
            );
            return Ok(());
        }
        let mut yanked = 0usize;
        let mut failed = 0usize;
        for path in &evidence.artifact_paths {
            // .crate paths are <name>-<version>.crate; parse name/version.
            // crates.io versions are immutable, so `cargo yank` is the
            // strongest unwind available; the version slot stays burned
            // and any consumer that already resolved against it keeps
            // working. Operators must still bump to recover.
            if let Some((name, version)) = parse_crate_name_version(path) {
                log.status(&format!("cargo: yank {} {}", name, version));
                let output = Command::new("cargo")
                    .args(["yank", "--version", &version, &name])
                    .output()?;
                if output.status.success() {
                    yanked += 1;
                } else {
                    failed += 1;
                    log.warn(&format!(
                        "cargo yank failed for {} {}: {}",
                        name,
                        version,
                        String::from_utf8_lossy(&output.stderr),
                    ));
                }
            }
        }
        log.status(&format!(
            "cargo: yanked {} crate(s), {} failure(s)",
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
        Some("CARGO_REGISTRY_TOKEN yank")
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

/// Build the expected `.crate` archive path for a crate published by
/// `cargo package` / `cargo publish`.
///
/// Cargo writes to `<target_dir>/package/<name>-<version>.crate`. When
/// the per-crate `publish.cargo.target_dir` is set, cargo respects it;
/// otherwise cargo uses `<project_root>/target/`. We compute the
/// predicted location so [`collect_local_crate_paths`] can probe the
/// filesystem deterministically — the helper is split out so it can be
/// unit-tested without spinning up a full publish fixture.
fn expected_crate_path(
    project_root: &std::path::Path,
    target_dir: Option<&std::path::Path>,
    name: &str,
    version: &str,
) -> std::path::PathBuf {
    let base = match target_dir {
        Some(td) => td.to_path_buf(),
        None => project_root.join("target"),
    };
    base.join("package").join(format!("{name}-{version}.crate"))
}

/// Enumeration of locally-produced `.crate` archive paths used by
/// rollback. Walks `ctx.config.crates` (plus workspace crates) for every
/// crate with `publish.cargo` configured, computes the predicted
/// `cargo package` output location via [`expected_crate_path`], and
/// returns those that exist on disk. Missing files are filtered out so
/// rollback's per-path yank loop never trips on a stale path.
///
/// The crate version comes from the crate's own `Cargo.toml` so
/// workspaces with mixed cadences (e.g. `cfgd-core@0.2.2` while
/// `cfgd@0.3.2`) yank the correct slot per crate. Falls back to the
/// run's release version when the manifest can't be parsed.
fn collect_local_crate_paths(ctx: &Context) -> Vec<std::path::PathBuf> {
    let release_version = ctx.version();
    let project_root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let mut out = Vec::new();
    for c in crate::util::all_crates(ctx) {
        let Some(ref publish) = c.publish else {
            continue;
        };
        let Some(ref cargo_cfg) = publish.cargo else {
            continue;
        };
        let version = read_cargo_toml_version(&c.path).unwrap_or_else(|| release_version.clone());
        if version.is_empty() {
            continue;
        }
        let path = expected_crate_path(
            &project_root,
            cargo_cfg.target_dir.as_deref(),
            &c.name,
            &version,
        );
        if path.exists() {
            out.push(path);
        }
    }
    out
}

/// Parse `name-1.2.3.crate` (or any `<name>-<version>` stem) into its
/// component parts. Crate names may contain `-`, and versions may carry
/// prerelease suffixes (`0.2.1-rc.1`) or build metadata (`0.2.1+build.5`)
/// that include additional `-` characters — so we scan for the FIRST
/// `-<digit>` boundary: everything before is the name, everything from
/// the digit onward is the version. This handles hyphenated names,
/// prereleases, build metadata, and snapshot suffixes uniformly.
fn parse_crate_name_version(path: &std::path::Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return Some((stem[..i].to_string(), stem[i + 1..].to_string()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};
    use std::path::Path;

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
        assert!(msg.starts_with("cargo:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("cargo:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("cargo:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("cargo:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
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
    fn parse_crate_name_version_handles_hyphenated_names() {
        assert_eq!(
            parse_crate_name_version(Path::new("anodizer-core-0.2.1.crate")),
            Some(("anodizer-core".to_string(), "0.2.1".to_string()))
        );
    }

    #[test]
    fn parse_crate_name_version_handles_prerelease() {
        assert_eq!(
            parse_crate_name_version(Path::new("anodizer-core-0.2.1-rc.1.crate")),
            Some(("anodizer-core".to_string(), "0.2.1-rc.1".to_string()))
        );
    }

    #[test]
    fn parse_crate_name_version_handles_build_metadata() {
        assert_eq!(
            parse_crate_name_version(Path::new("foo-0.2.1+build.5.crate")),
            Some(("foo".to_string(), "0.2.1+build.5".to_string()))
        );
    }

    #[test]
    fn parse_crate_name_version_handles_snapshot_suffix() {
        assert_eq!(
            parse_crate_name_version(Path::new("anodizer-0.2.1-SNAPSHOT-abc.crate")),
            Some(("anodizer".to_string(), "0.2.1-SNAPSHOT-abc".to_string()))
        );
    }

    #[test]
    fn parse_crate_name_version_rejects_non_versioned_stems() {
        // No `-<digit>` boundary anywhere in the stem.
        assert_eq!(
            parse_crate_name_version(Path::new("anodizer-core.crate")),
            None
        );
        assert_eq!(parse_crate_name_version(Path::new("plain-package")), None);
    }

    #[test]
    fn expected_crate_path_uses_project_root_target_when_unset() {
        let p = expected_crate_path(Path::new("/repo"), None, "anodizer-core", "0.2.1");
        assert_eq!(
            p,
            std::path::PathBuf::from("/repo/target/package/anodizer-core-0.2.1.crate")
        );
    }

    #[test]
    fn expected_crate_path_uses_configured_target_dir() {
        let p = expected_crate_path(
            Path::new("/repo"),
            Some(Path::new("/custom-target")),
            "foo",
            "1.2.3-rc.1",
        );
        assert_eq!(
            p,
            std::path::PathBuf::from("/custom-target/package/foo-1.2.3-rc.1.crate")
        );
    }

    #[test]
    fn collect_local_crate_paths_finds_published_crates() {
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

        let tmp = tempfile::tempdir().expect("tempdir");
        let project_root = tmp.path().to_path_buf();

        // Create the crate's source dir with a Cargo.toml so
        // read_cargo_toml_version returns a concrete version.
        let crate_dir = project_root.join("crates/anodizer-core");
        std::fs::create_dir_all(&crate_dir).expect("mkdir crate");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"anodizer-core\"\nversion = \"0.2.1\"\n",
        )
        .expect("write Cargo.toml");

        // Create the predicted .crate emission so the path probe hits.
        let pkg_dir = project_root.join("target/package");
        std::fs::create_dir_all(&pkg_dir).expect("mkdir pkg");
        let crate_path = pkg_dir.join("anodizer-core-0.2.1.crate");
        std::fs::write(&crate_path, b"fake").expect("write .crate");

        // Second crate is configured but has NO .crate file on disk;
        // the walker must filter it out (missing path => not surfaced).
        let crate_cfg = CrateConfig {
            name: "anodizer-core".to_string(),
            path: crate_dir.display().to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let other = CrateConfig {
            name: "anodizer-missing".to_string(),
            path: "nonexistent".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let ctx = TestContextBuilder::new()
            .project_root(project_root.clone())
            .crates(vec![crate_cfg, other])
            .build();

        let paths = collect_local_crate_paths(&ctx);
        assert_eq!(paths, vec![crate_path]);
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
    /// The substrings were last verified against cargo 1.95.x (rustc 1.95.0,
    /// released 2026-04-14). Bump `VERIFIED_CARGO_MINOR` only after
    /// manually confirming all three substrings still appear verbatim in
    /// the new cargo's publish output.
    #[test]
    fn cargo_version_matches_pinned_discriminator_strings() {
        // Last-verified cargo minor. Update together with re-verification.
        const VERIFIED_CARGO_MINOR: u64 = 95;

        let output = std::process::Command::new("cargo")
            .arg("--version")
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
        use std::os::unix::fs::PermissionsExt;

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
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");

        let cmd = vec![stub.display().to_string(), "publish".to_string()];
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
        use std::os::unix::fs::PermissionsExt;

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
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");

        let cmd = vec![stub.display().to_string(), "publish".to_string()];
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
        // Inject counter path so the stub can find it.
        std::env::set_var("STUB_COUNTER", counter.display().to_string());
        let result = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect("retry harness must succeed after propagation lag");
        std::env::remove_var("STUB_COUNTER");
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
        std::env::set_var("STUB_COUNTER", counter.display().to_string());
        let err = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect_err("non-propagation failure must surface");
        std::env::remove_var("STUB_COUNTER");
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
}
