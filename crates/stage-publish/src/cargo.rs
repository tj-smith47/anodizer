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
/// `--allow-dirty` is implicit. The release (Publish) job checks out the
/// committed release tag — a CLEAN tree (version bump + Cargo.lock were
/// committed at tag time). The only thing that dirties the tree before publish
/// is anodizer's own `[package.metadata.binstall]` write (an idempotent,
/// uncommitted refresh emitted just before `cargo publish`). `--allow-dirty`
/// lets that uncommitted write through; without it `cargo publish` would reject
/// the binstall-enabled crate. Users can still set `cargo.allow_dirty: false`
/// to opt out, but that's surprising enough we force-on by default.
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
    // allow_dirty defaults to ON when unset: the publish runs from a clean tag
    // checkout, but anodizer's own binstall metadata write dirties the tree
    // just before publish, and `cargo publish` would otherwise reject it.
    // Setting `allow_dirty: false` explicitly disables it.
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
    format!("https://index.crates.io/{}", sparse_index_path(crate_name))
}

/// The crates.io web-API base for the token-validity probe (`/api/v1/me`).
///
/// Mirrors the sparse-index base override in [`published_on_crates_io`]:
/// integration tests drive the real binary across a process boundary, so an
/// env-routed base pointing at a local responder is the only way to keep the
/// live token probe hermetic there. Honored ONLY under `ANODIZE_TEST_HARNESS=1`
/// so no production run can point the credential probe at a friendly endpoint.
fn crates_io_api_base() -> String {
    match std::env::var("ANODIZER_TEST_CRATES_IO_API_BASE") {
        Ok(base) if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") => {
            base.trim_end_matches('/').to_string()
        }
        _ => "https://crates.io".to_string(),
    }
}

/// The registry-relative sparse-index path for a crate (`1/a`, `2/ab`,
/// `3/a/abc`, `ab/cd/abcdef`), shared by [`sparse_index_url`] and the
/// test-harness index-base override in [`published_on_crates_io`] so the
/// sharding scheme exists exactly once.
fn sparse_index_path(crate_name: &str) -> String {
    debug_assert!(
        crate_name.is_ascii(),
        "cargo crate names must be ASCII; got {crate_name:?}"
    );
    let lower = crate_name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("1/{}", lower),
        2 => format!("2/{}", lower),
        3 => format!("3/{}/{}", &lower[..1], lower),
        _ => format!("{}/{}/{}", &lower[..2], &lower[2..4], lower),
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
    log: &StageLogger,
) -> Result<Option<String>> {
    is_already_published_at(
        &sparse_index_url(crate_name),
        crate_name,
        version,
        policy,
        log,
    )
}

/// Same as [`is_already_published`] but uses the supplied URL instead of
/// computing one from `sparse_index_url`. Lets tests point at a local TCP
/// responder so the retry plumbing can be exercised end-to-end.
fn is_already_published_at(
    url: &str,
    crate_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Option<String>> {
    use anodizer_core::retry::{SuccessClass, retry_http_blocking};
    use std::time::Duration;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index check")?;

    let label = format!("publish: query crates.io index for '{}'", crate_name);
    let result = retry_http_blocking(
        anodizer_core::retry::RetryLog::new(&label, log),
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

/// Probe crates.io's sparse index for whether `name` at `version` is
/// published — the GLOBAL registry answer, independent of any single run's
/// evidence. `Ok(true)` = the version is live (burned — crates.io never
/// accepts the same version twice), `Ok(false)` = positively absent (index
/// 404 or version missing from the index body), `Err` = the index could not
/// be consulted (callers making destructive decisions must FAIL CLOSED on
/// this).
///
/// Public so failure-recovery tooling (`tag rollback`'s published-state
/// guard) reuses the same sparse-index client + JSONL parser the publish
/// stage trusts, instead of growing a second index parser.
pub fn published_on_crates_io(
    name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    // Test-harness index-base override, mirroring `--simulate-failure`'s env
    // gating: integration tests drive the real binary across a process
    // boundary, so an env-routed base pointing at a local responder is the
    // only way to keep this probe hermetic there. Honored ONLY under
    // ANODIZE_TEST_HARNESS=1 so no production run can point the
    // published-state guard at a friendly index.
    let url = match std::env::var("ANODIZER_TEST_CRATES_IO_INDEX_BASE") {
        Ok(base) if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") => {
            format!("{}/{}", base.trim_end_matches('/'), sparse_index_path(name))
        }
        _ => sparse_index_url(name),
    };
    Ok(is_already_published_at(&url, name, version, policy, log)?.is_some())
}

/// Whether a crate's resolved `publish.cargo` block targets the default
/// crates.io registry, where the sparse-index cksum the content-vs-version
/// guard compares against is authoritative.
///
/// A custom `registry =`/`index =` points cargo at a different index, so the
/// crates.io cksum the guard fetched describes a DIFFERENT artifact (or none).
/// The content guard — and the already-published idempotency skip itself —
/// only hold against the registry actually being published to, so both are
/// disabled for non-crates.io targets and the publish is attempted (the
/// target registry's own server-side conflict handling governs idempotency).
///
/// Public for the same reason as [`published_on_crates_io`]: `tag rollback`'s
/// published-state guard must scope its crates.io probe with the same
/// judgment the publisher applies.
pub fn targets_crates_io(cfg: Option<&CargoPublishConfig>) -> bool {
    match cfg {
        None => true,
        Some(c) => c.registry.is_none() && c.index.is_none(),
    }
}

/// Whether anodizer's changelog stage (re)generates on-disk `CHANGELOG.md`
/// files under this run's config — the condition under which a crate-root
/// `CHANGELOG.md` difference against an already-published version is
/// anodizer's own re-cut artifact rather than operator-authored drift (see
/// [`crates_equal_modulo_vcs`]).
///
/// Mirrors the gates `ChangelogStage::run` applies before writing files (a
/// deliberate pairing — keep the two in sync when the stage grows a new
/// gate):
/// - a `changelog:` block must be configured,
/// - the stage must not be `--skip`ped,
/// - snapshot mode skips the stage unless `changelog.snapshot: true`,
/// - `use: github-native` delegates the release body to GitHub's API and
///   writes no on-disk changelog files,
/// - a truthy `changelog.skip` template turns the stage off. An unrenderable
///   template also counts as inactive: the guard then stays byte-strict on
///   CHANGELOG.md (fail closed) instead of forgiving drift it cannot prove
///   the tool produced.
fn changelog_stage_regenerates_files(ctx: &Context) -> bool {
    let Some(cfg) = ctx.config.changelog.as_ref() else {
        return false;
    };
    if ctx.should_skip("changelog") {
        return false;
    }
    if ctx.is_snapshot() && !cfg.resolved_snapshot() {
        return false;
    }
    if cfg.resolved_use_source() == "github-native" {
        return false;
    }
    if let Some(d) = cfg.skip.as_ref() {
        match d.try_evaluates_to_true(|s| ctx.render_template(s)) {
            Ok(false) => {}
            Ok(true) | Err(_) => return false,
        }
    }
    true
}

/// Local `.crate` package produced by [`local_crate_cksum`]: the sha256 the
/// fast path compares against the crates.io index `cksum`, plus the raw
/// tarball bytes the slow path (`crates_equal_modulo_vcs`) needs when the
/// fast path doesn't match.
struct LocalCrate {
    cksum: String,
    bytes: Vec<u8>,
}

/// Package `crate_name` locally and return the lowercase-hex sha256 of the
/// produced `.crate` tarball, plus the tarball bytes — the same digest
/// crates.io records as a version's `cksum`. Used by the content-vs-version
/// guard to prove a re-cut of an already-published version is byte-identical
/// (or identical modulo `.cargo_vcs_info.json`) to what shipped.
///
/// Returns `Ok(None)` when the crate does not target crates.io (the guard is
/// inapplicable — see [`targets_crates_io`]). Returns `Err` when packaging or
/// hashing fails: the caller treats that as a fail-closed condition, never a
/// safe skip, because an uncomputable local digest cannot prove content
/// identity against an immutable published version.
///
/// No `SOURCE_DATE_EPOCH` is set: `cargo package` does not consult it for the
/// `.crate` tarball's bytes (the mtimes it writes are cargo's own canonical
/// constant, independent of the env var), so the local `.crate` reproduces the
/// bytes the original `cargo publish` uploaded purely from identical source at
/// the same commit. `cargo package` also embeds the release `git.sha1` in
/// `.cargo_vcs_info.json`, so a re-cut from a DIFFERENT commit changes the
/// tarball bytes even with identical sources — `decide_already_published`'s
/// slow path (`crates_equal_modulo_vcs`) is what tells that apart from a real
/// content change. Seeding `SOURCE_DATE_EPOCH` would be a no-op that misleads
/// a reader into thinking it is load-bearing.
fn local_crate_cksum(
    crate_name: &str,
    crate_cfg: &CrateConfig,
    cargo_cfg: Option<&CargoPublishConfig>,
    log: &StageLogger,
) -> Result<Option<LocalCrate>> {
    if !targets_crates_io(cargo_cfg) {
        return Ok(None);
    }

    let manifest_dir = std::path::Path::new(&crate_cfg.path);
    // Hermetic target dir so the produced `.crate` is isolated from any
    // workspace `target/` and trivially discoverable.
    let pkg_target = tempfile::tempdir().context("publish: tempdir for local .crate package")?;

    let mut env: HashMap<String, String> = HashMap::new();
    // Inherit PATH (cargo + toolchain lookup), HOME/CARGO_HOME (registry
    // config), and RUSTUP_HOME (toolchain resolution) so the packaging step
    // resolves the registry and toolchain the same way the real publish does.
    // env_clear in package_one wipes the rest.
    for key in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_string(), v);
        }
    }
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        pkg_target.path().display().to_string(),
    );

    anodizer_core::cargo_package::package_one(crate_name, manifest_dir, &env, log).with_context(
        || format!("publish: package '{crate_name}' to compute local .crate cksum"),
    )?;

    let version = read_cargo_toml_version(&crate_cfg.path).unwrap_or_default();
    let crate_file = pkg_target
        .path()
        .join("package")
        .join(format!("{crate_name}-{version}.crate"));
    let cksum = anodizer_core::hashing::sha256_file(&crate_file)
        .with_context(|| format!("publish: sha256 local .crate for '{crate_name}'"))?;
    let bytes = std::fs::read(&crate_file)
        .with_context(|| format!("publish: read local .crate bytes for '{crate_name}'"))?;
    Ok(Some(LocalCrate { cksum, bytes }))
}

/// Refuse to run the content-vs-version poison guard against a dirty working
/// tree.
///
/// `cargo package` stamps `"dirty": true` into the `.crate`'s
/// `.cargo_vcs_info.json` whenever the tree differs from `HEAD`, which changes
/// the tarball bytes. The release (Publish) job checks out the committed tag —
/// a clean tree — so reproduction holds there. A manual `--publish-only` from a
/// DIRTY operator workspace would package dirty bytes and (a) false-poison a
/// crate that was published clean, or (b) mask real content drift behind the
/// dirty marker. Either way the comparison against the immutable index cksum is
/// no longer trustworthy.
///
/// Called ONCE before the publish loop's first binstall write, so anodizer's
/// own (expected) binstall mutation is not itself flagged. Fails loud rather
/// than silently skipping (a poison hole) or hard-failing on content (which
/// would misattribute the divergence to a code change). The message lists the
/// dirty paths and prescribes re-running from a clean tag checkout.
fn ensure_publish_tree_clean(ctx: &Context) -> Result<()> {
    let repo = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let porcelain = match anodizer_core::git::git_status_porcelain_result_in(&repo) {
        Ok(out) => out,
        // An errored `git status` (non-repo cwd, git absent, locked index)
        // cannot PROVE the tree is clean. A guard that "fails loud rather than
        // silently skipping" must refuse here, never treat the indeterminate
        // result as clean — that would be the very poison hole this gate closes.
        Err(e) => anyhow::bail!(
            "publish: cannot verify the working tree is clean before checking already-published \
             crates against the crates.io index ({e:#}). Without a clean-tree proof, a local \
             `cargo package` checksum cannot be trusted to match what was published from the \
             release tag. Re-run the publish from a clean git checkout of the release tag (the \
             Release job does this automatically)."
        ),
    };
    if porcelain.trim().is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "publish: working tree is DIRTY before verifying already-published crates against the \
         crates.io index. `cargo package` records the dirty state in the .crate, so a local \
         checksum would NOT match what was published from the clean release tag — \
         already-published content verification is unreliable. Re-run from a clean checkout of \
         the release tag (the Release job does this automatically; `git status` must show no \
         changes). Uncommitted changes:\n{porcelain}"
    );
}

/// Fetch the published `.crate` tarball bytes for `{name}-{version}` from
/// crates.io's static CDN — the canonical immutable per-version artifact
/// path (mirrors how `cargo` itself downloads dependencies). Used by the
/// content-vs-version guard's slow path when the local `.crate` doesn't
/// byte-match the index cksum, to prove (or disprove) that the mismatch is
/// only the `.cargo_vcs_info.json` commit stamp.
///
/// Routes through [`retry_http_blocking_bytes`] (not the text-bodied
/// `retry_http_blocking`): a `.crate` is a gzip tarball, and `resp.text()`'s
/// lossy UTF-8 pass would corrupt it silently.
fn fetch_published_crate(
    name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Vec<u8>> {
    use anodizer_core::retry::{SuccessClass, retry_http_blocking_bytes};
    use std::time::Duration;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("publish: build HTTP client for published .crate fetch")?;
    let url = format!("https://static.crates.io/crates/{name}/{name}-{version}.crate");
    let label = format!("publish: fetch published .crate for '{name}-{version}'");
    let (_status, bytes) = retry_http_blocking_bytes(
        anodizer_core::retry::RetryLog::new(&label, log),
        policy,
        SuccessClass::Strict,
        |_| client.get(&url).send(),
        |status, body| {
            format!(
                "publish: crates.io static CDN returned {} for '{name}-{version}': {}",
                status,
                redact_bearer_tokens(body)
            )
        },
    )?;
    Ok(bytes)
}

/// A release-process normalization [`crates_equal_modulo_vcs`] applied to
/// forgive a byte difference in one crate-root entry. These are the ONLY
/// files whose drift the guard may attribute to anodizer's own release
/// machinery; the set is built in on purpose — a user-facing ignore list
/// would let a config knob reopen the content-drift poison hole the guard
/// exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecutNormalization {
    /// Crate-root `.cargo_vcs_info.json`, compared modulo its `git.sha1`
    /// field (the release commit stamp of a same-source re-cut).
    VcsCommitStamp,
    /// Crate-root `CHANGELOG.md`, forgiven because the changelog stage is
    /// active for this run and regenerates the file on every re-cut.
    ChangelogRegenerated,
    /// Crate-root `Cargo.lock` on a crate with no binary targets: cargo
    /// ignores a dependency's packaged lockfile for library consumers, so
    /// lockfile-only drift cannot change what any consumer builds.
    LockfileLibOnly,
}

impl RecutNormalization {
    /// Operator-facing description for the clean-skip log line.
    fn describe(self) -> &'static str {
        match self {
            Self::VcsCommitStamp => ".cargo_vcs_info.json (commit stamp)",
            Self::ChangelogRegenerated => "CHANGELOG.md (regenerated by the changelog stage)",
            Self::LockfileLibOnly => "Cargo.lock (lib-only crate)",
        }
    }
}

/// Outcome of comparing a local `.crate` tarball against the published one,
/// modulo anodizer's own release-process artifacts (see
/// [`RecutNormalization`]).
#[derive(Debug)]
enum CrateContentMatch {
    /// Every entry matches, except release-process artifacts the comparison
    /// normalized; `normalized` enumerates exactly which rules fired (empty
    /// when the archives are byte-identical).
    Equivalent { normalized: Vec<RecutNormalization> },
    /// At least one entry genuinely differs; lists the differing paths (with
    /// a why-not-normalized annotation for conditionally-normalizable files)
    /// so an operator can see what drifted and why it was not forgiven.
    Differs(Vec<String>),
}

/// Untar a gzip-compressed `.crate` tarball into `path -> bytes`, keyed by
/// the archive's own in-tar path (e.g. `{name}-{version}/src/lib.rs`).
fn read_crate_entries(
    crate_bytes: &[u8],
) -> Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
    use std::io::Read as _;

    let decoder = flate2::read::GzDecoder::new(crate_bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = std::collections::BTreeMap::new();
    for entry in archive
        .entries()
        .context("publish: read .crate tar entries")?
    {
        let mut entry = entry.context("publish: read .crate tar entry")?;
        let path = entry
            .path()
            .context("publish: read .crate entry path")?
            .into_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("publish: read .crate entry '{}'", path.display()))?;
        entries.insert(path, bytes);
    }
    Ok(entries)
}

/// Parse `.cargo_vcs_info.json` bytes and strip the `git.sha1` field — the
/// one field that legitimately differs between two same-source re-cuts (it
/// records the commit `cargo package` ran at, not the packaged sources).
///
/// Returns `None` when the bytes don't parse as JSON — the caller then falls
/// back to a raw byte compare, which fails closed (real drift) rather than
/// silently treating unparseable metadata as a match.
fn vcs_info_modulo_sha(bytes: &[u8]) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if let Some(git) = value.get_mut("git")
        && let Some(obj) = git.as_object_mut()
    {
        obj.remove("sha1");
    }
    Some(value)
}

/// Whether `path` is the named file at the crate root of a `.crate` tarball
/// (`{name}-{version}/<file_name>`).
///
/// The release-process normalizations apply ONLY at exactly this position: a
/// file with the same basename anywhere deeper is ordinary packaged source
/// and must be byte-compared, or a real source change hiding under a
/// well-known name would be masked into a false skip. Count only Normal
/// components so a leading `./` (a CurDir component) can't inflate the count
/// and misclassify the root file as nested source; cargo's .crate tarballs
/// never emit `./`-prefixed entries, but the gate shouldn't depend on that
/// external emission detail.
fn is_crate_root_entry(path: &std::path::Path, file_name: &str) -> bool {
    let normal_component_count = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    normal_component_count == 2 && path.file_name().is_some_and(|f| f == file_name)
}

/// Whether the packaged crate carries any installable target — binaries
/// AND examples — judged from the LOCAL tarball's entries. Examples count
/// because `cargo install --example` consumes the packaged lockfile
/// exactly like `cargo install` does for bins, so a lib-with-examples
/// crate's lockfile is consumer-visible and must stay byte-strict.
///
/// Source of truth: the normalized crate-root `Cargo.toml` INSIDE the
/// tarball. `cargo package` rewrites the packaged manifest with explicit
/// target sections (auto-discovered bins/examples become literal `[[bin]]`
/// / `[[example]]` tables), so the packaged manifest — unlike the
/// workspace-relative source manifest — states target-ness explicitly,
/// needs no `cargo metadata` subprocess, and describes exactly the
/// artifact being compared (the tarball bytes are already in memory).
/// Local vs published manifests are interchangeable here: a Cargo.toml
/// byte difference is never normalizable, so the caller hard-fails before
/// this answer matters.
///
/// Belt-and-braces: conventional installable source paths (`src/main.rs`,
/// `src/bin/**`, `examples/**`) in the tarball also count, guarding
/// against a manifest normalization scheme that leaves auto-discovery
/// implicit. The supplement only ever WIDENS "has installable targets"
/// (tightening the guard toward byte-strict), never widens "lib-only".
///
/// Returns `None` when the root Cargo.toml is missing or unparseable — the
/// caller fails closed (byte-strict Cargo.lock) on an indeterminate answer.
fn packaged_crate_has_bin_targets(
    entries: &std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
) -> Option<bool> {
    let manifest_bytes = entries
        .iter()
        .find(|(p, _)| is_crate_root_entry(p, "Cargo.toml"))
        .map(|(_, b)| b)?;
    let manifest = std::str::from_utf8(manifest_bytes).ok()?;
    let doc = manifest.parse::<toml_edit::DocumentMut>().ok()?;
    // A key of an unrecognized shape counts as targets-exist so the
    // lockfile stays byte-strict (fail closed).
    let explicit_targets = ["bin", "example"].iter().any(|key| match doc.get(key) {
        None => false,
        Some(item) => item
            .as_array_of_tables()
            .map(|t| !t.is_empty())
            .or_else(|| item.as_array().map(|a| !a.is_empty()))
            .unwrap_or(true),
    });
    let conventional_installable_sources = entries.keys().any(|p| {
        let mut normals = p
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .skip(1); // {name}-{version}/ root dir
        matches!(
            (normals.next(), normals.next()),
            (Some("src"), Some("main.rs")) | (Some("src"), Some("bin")) | (Some("examples"), _)
        )
    });
    Some(explicit_targets || conventional_installable_sources)
}

/// Pure, unit-testable comparison at the heart of the content-vs-version
/// guard's slow path: are `local_crate` and `published_crate` the SAME
/// published sources, differing only in anodizer's own release-process
/// artifacts?
///
/// Both inputs are `.crate` files (gzip-compressed tarballs). Two crates
/// represent the same published sources iff, for every tar entry path, the
/// bytes are equal, EXCEPT (crate-root position only — see
/// [`is_crate_root_entry`]):
///
/// - `.cargo_vcs_info.json` — compared modulo its `git.sha1` field (a
///   legitimate per-commit delta — see [`local_crate_cksum`]).
/// - `CHANGELOG.md` — forgiven ONLY when `changelog_stage_active` is true:
///   anodizer's changelog stage regenerates the file on every re-cut, so
///   the drift is the tool's own artifact. With no changelog stage in play
///   the drift is operator-authored and stays a hard divergence.
/// - `Cargo.lock` — forgiven ONLY for lib-only crates (no binary or
///   example targets; see [`packaged_crate_has_bin_targets`]): cargo
///   ignores a dependency's packaged lockfile for library consumers, but
///   `cargo install --locked` (for bins) and `cargo install --locked
///   --example` (for examples) make it consumer-visible, so it stays
///   byte-strict there.
///
/// A file present in only one archive is always an unambiguous divergence,
/// even for the normalizable names. Every other entry — `Cargo.toml`
/// included — is byte-compared; the equivalence set is deliberately BUILT
/// IN with no config knob, so consumers cannot widen it into a poison hole.
fn crates_equal_modulo_vcs(
    local_crate: &[u8],
    published_crate: &[u8],
    changelog_stage_active: bool,
) -> Result<CrateContentMatch> {
    let local_entries = read_crate_entries(local_crate)
        .context("publish: unpack local .crate for content comparison")?;
    let published_entries = read_crate_entries(published_crate)
        .context("publish: unpack published .crate for content comparison")?;

    let mut differs = Vec::new();
    let mut normalized = Vec::new();
    let all_paths: std::collections::BTreeSet<&std::path::PathBuf> = local_entries
        .keys()
        .chain(published_entries.keys())
        .collect();

    for path in all_paths {
        let path_str = path.display().to_string();
        let (Some(local_bytes), Some(published_bytes)) =
            (local_entries.get(path), published_entries.get(path))
        else {
            // Present in only one archive — an unambiguous content divergence.
            differs.push(path_str);
            continue;
        };
        if local_bytes == published_bytes {
            continue;
        }

        if is_crate_root_entry(path, ".cargo_vcs_info.json") {
            match (
                vcs_info_modulo_sha(local_bytes),
                vcs_info_modulo_sha(published_bytes),
            ) {
                (Some(l), Some(p)) if l == p => {
                    normalized.push(RecutNormalization::VcsCommitStamp);
                }
                // Either side failed to parse, or a field OTHER than git.sha1
                // differs — a structural change beyond the commit stamp is
                // real drift.
                _ => differs.push(path_str),
            }
        } else if is_crate_root_entry(path, "CHANGELOG.md") {
            if changelog_stage_active {
                normalized.push(RecutNormalization::ChangelogRegenerated);
            } else {
                differs.push(format!(
                    "{path_str} (not treated as a release-process artifact: no changelog \
                     stage is configured/enabled for this run, so anodizer did not \
                     regenerate this file and the drift is real)"
                ));
            }
        } else if is_crate_root_entry(path, "Cargo.lock") {
            match packaged_crate_has_bin_targets(&local_entries) {
                Some(false) => normalized.push(RecutNormalization::LockfileLibOnly),
                Some(true) => differs.push(format!(
                    "{path_str} (not treated as a release-process artifact: the crate has \
                     binary or example targets, so the packaged lockfile is \
                     consumer-visible via `cargo install --locked` (or `--example`) and \
                     stays byte-strict)"
                )),
                None => differs.push(format!(
                    "{path_str} (not treated as a release-process artifact: could not \
                     determine binary/example targets from the packaged Cargo.toml, so \
                     the lockfile stays byte-strict)"
                )),
            }
        } else {
            differs.push(path_str);
        }
    }

    if differs.is_empty() {
        Ok(CrateContentMatch::Equivalent { normalized })
    } else {
        Ok(CrateContentMatch::Differs(differs))
    }
}

/// Outcome of the already-published content-vs-version poison guard for one
/// crate. `Skip` means the version is on crates.io with content identical to
/// (or source-equivalent modulo release-process artifacts vs — see
/// [`crates_equal_modulo_vcs`]) the local `.crate` — a safe idempotent
/// re-cut; `Publish` means proceed to `cargo publish`. A poisoned version
/// (published with genuinely DIFFERENT content) never reaches either arm —
/// the guard hard-fails instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoSkipDecision {
    Skip,
    Publish,
}

/// Decide whether an already-published crates.io version is a safe skip.
///
/// 1. **Fast path:** local `.crate` sha256 == index `cksum` → byte-identical
///    re-cut → [`CargoSkipDecision::Skip`], no download.
/// 2. **Slow path** (local sha256 != index `cksum`): fetch the published
///    `.crate` via `fetch_published`, verify ITS sha256 matches `index_cksum`
///    (a mismatched download is not a valid comparison basis — fail closed),
///    then compare local vs published with [`crates_equal_modulo_vcs`].
///    The equivalence set is built in (never user-configurable) and covers,
///    at the crate-root position only:
///    - `.cargo_vcs_info.json` modulo `git.sha1` (always);
///    - `CHANGELOG.md` (only when `changelog_stage_active` — anodizer's own
///      changelog stage regenerates it between re-cuts);
///    - `Cargo.lock` (only for lib-only crates — binary crates expose the
///      packaged lockfile to consumers via `cargo install --locked`).
///
///    Source-equivalent → `Skip`, with a log line enumerating exactly which
///    normalizations applied; genuinely different → HARD FAIL, naming the
///    differing entry paths (annotated with WHY a conditionally-normalizable
///    file was not forgiven when the condition did not hold).
/// 3. index cksum is empty → cannot verify content identity → FAIL CLOSED. An
///    empty cksum on an index entry the parser DID return signals a
///    malformed/unparsed index line, not a benign registry gap (crates.io
///    always records a cksum). Silently skipping it would reopen the poison
///    hole this guard exists to close.
/// 4. `local_crate_check` returns `Ok(None)` → no local digest was produced
///    for a crates.io-targeting crate (the caller already routes
///    non-crates.io targets to `Publish`, so reaching here is unexpected) →
///    FAIL CLOSED rather than skip an unverifiable version.
/// 5. `local_crate_check` returns `Err`, or `fetch_published` returns `Err` →
///    FAIL CLOSED: an uncomputable local digest, or an unreachable published
///    artifact, cannot prove content identity against an immutable published
///    version, so refuse to skip.
///
/// `Skip` is therefore reachable ONLY via a confirmed byte-identical match or
/// a confirmed source-equivalent match (release-process artifacts only)
/// against the verified published artifact; every other ("cannot verify")
/// outcome fails closed.
#[allow(clippy::too_many_arguments)]
fn decide_already_published(
    name: &str,
    version: &str,
    index_cksum: &str,
    crate_cfg: &CrateConfig,
    cargo_cfg: Option<&CargoPublishConfig>,
    changelog_stage_active: bool,
    local_crate_check: impl Fn(
        &str,
        &CrateConfig,
        Option<&CargoPublishConfig>,
    ) -> Result<Option<LocalCrate>>,
    fetch_published: impl Fn(&str, &str) -> Result<Vec<u8>>,
    log: &StageLogger,
) -> Result<CargoSkipDecision> {
    if index_cksum.is_empty() {
        anyhow::bail!(
            "publish: crates.io index entry for '{name}-{version}' carries no cksum, so its \
             content identity cannot be verified. Refusing to skip — an empty cksum on an \
             existing index entry signals a malformed/unparsed index line, not a safe re-cut, \
             and silently skipping it would reopen the content-drift poison hole this guard \
             closes. Re-run once the crates.io index is fully reachable, or bump the version."
        );
    }

    let local = match local_crate_check(name, crate_cfg, cargo_cfg) {
        Ok(None) => anyhow::bail!(
            "publish: '{name}-{version}' is published on crates.io but no local .crate checksum \
             was produced to compare against (the crate targets crates.io, so a digest was \
             expected). Refusing to skip a version whose content identity is unverifiable."
        ),
        Ok(Some(local)) => local,
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "publish: '{name}-{version}' is already published on crates.io but its local \
                     .crate checksum could not be computed; refusing to skip a version that may \
                     have drifted from the published content. Resolve the packaging error and \
                     re-run, or bump the version."
                )
            });
        }
    };

    // Fast path: byte-identical tarball → no download needed.
    if local.cksum.eq_ignore_ascii_case(index_cksum) {
        log.verbose(&format!(
            "'{name}-{version}' local .crate checksum matches the crates.io \
             index ({index_cksum}); safe idempotent re-cut"
        ));
        return Ok(CargoSkipDecision::Skip);
    }

    // Slow path: the local tarball's sha256 doesn't match the index cksum,
    // which is exactly what a same-source re-cut from a NEW commit looks
    // like (cargo package embeds the release git sha in
    // .cargo_vcs_info.json). Fetch the published artifact and compare
    // modulo that one file before concluding real content drift.
    let published_bytes = fetch_published(name, version).with_context(|| {
        format!(
            "publish: '{name}-{version}' is already published on crates.io with a local .crate \
             checksum that does not match the index (index cksum {index_cksum}, local .crate \
             cksum {}), but the published .crate could not be fetched to rule out a same-source \
             re-cut. Refusing to skip a version whose content identity is unverifiable.",
            local.cksum
        )
    })?;

    use sha2::Digest as _;
    let published_cksum =
        anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(&published_bytes));
    if !published_cksum.eq_ignore_ascii_case(index_cksum) {
        anyhow::bail!(
            "publish: '{name}-{version}' is already published on crates.io, but the .crate \
             fetched from the static CDN has sha256 {published_cksum}, which does NOT match the \
             index-recorded cksum {index_cksum}. A mismatched download cannot be used to verify \
             content identity — refusing to skip. Re-run once crates.io is consistent, or bump \
             the version."
        );
    }

    match crates_equal_modulo_vcs(&local.bytes, &published_bytes, changelog_stage_active)? {
        CrateContentMatch::Equivalent { normalized } => {
            // `normalized` is non-empty whenever this arm is reached from the
            // real pipeline (byte-identical archives take the fast path), but
            // an injected local cksum in tests can land here with no delta —
            // describe that honestly rather than index into an empty list.
            let applied = if normalized.is_empty() {
                "none (archives are byte-identical)".to_string()
            } else {
                normalized
                    .iter()
                    .map(|n| n.describe())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            log.verbose(&format!(
                "'{name}-{version}' local .crate differs from the published crate only in \
                 release-process artifacts: {applied} — source-equivalent re-cut, safe \
                 idempotent skip"
            ));
            Ok(CargoSkipDecision::Skip)
        }
        CrateContentMatch::Differs(paths) => {
            anyhow::bail!(
                "publish: '{name}-{version}' is ALREADY published on crates.io with DIFFERENT \
                 content (index cksum {index_cksum}, local .crate cksum {}). Re-publishing would \
                 be SILENTLY SKIPPED by cargo, so the changed code would never ship under this \
                 version. Differing entries: {}. Bump the version (crates.io versions are \
                 immutable) and re-run.",
                local.cksum,
                paths.join(", ")
            );
        }
    }
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
    use std::time::Duration;
    // First SLEEP is 1s, not 5s: the sparse index frequently propagates a
    // freshly-published crate within 1-2s, so a hard 5s floor wastes the
    // common case. The first probe is free (no wait), and the early backoff
    // doubles 1→2→4→8… capped at MAX_POLL_DELAY, so a slow-propagating index
    // still backs off promptly without hammering the endpoint.
    poll_crates_io_index_at(
        &sparse_index_url(crate_name),
        crate_name,
        version,
        timeout_secs,
        Duration::from_secs(1),
        log,
    )
}

/// Same as [`poll_crates_io_index`] but uses the supplied URL and initial
/// back-off instead of computing them. Lets tests point at a local TCP
/// responder and skip the production 5 s first delay.
fn poll_crates_io_index_at(
    url: &str,
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    initial_backoff: std::time::Duration,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    const MAX_POLL_DELAY: Duration = Duration::from_secs(60);

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index polling")?;

    let mut backoff = initial_backoff;

    // Per-attempt logs go to `debug` — transient HTTP errors are the
    // normal shape of "the index hasn't propagated yet"; surfacing them
    // at `warn`/`error` floods normal release output. The terminal
    // timeout below escalates with a single bail!() carrying the same
    // context the per-attempt logs would have shown.
    loop {
        match client.get(url).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = anodizer_core::http::body_of_blocking(resp);
                // Each line of the sparse index is a JSON object; parse and check vers field.
                if body.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|v| v.get("vers")?.as_str().map(|s| s == version))
                        .unwrap_or(false)
                }) {
                    log.verbose(&format!(
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

/// Whether `cargo publish` failed with a transient network/transport fault a
/// retry can plausibly recover from.
///
/// Distinct from [`is_index_propagation_failure`] (sparse-index edge skew):
/// these are TCP/TLS/HTTP transport faults talking to crates.io or its CDN.
/// `cargo publish` makes live network calls mid-run — the registry index
/// update and the verification dep-download — and cargo retries "spurious"
/// transport errors a few times internally, but that budget is exhaustible.
/// A v0.11.3 release cut died with `[16] Error in the HTTP2 framing layer`
/// after cargo's own retries ran out, aborting the sequential workspace
/// publish 12 crates in and burning the re-cut attempt. An outer bounded
/// retry with backoff recovers from a momentary blip without masking real
/// failures: auth, packaging, and validation errors do NOT match here and
/// fast-fail unchanged on the first attempt.
///
/// Matched case-insensitively because curl and cargo vary the casing of the
/// same underlying fault across versions and platforms.
fn is_transient_network_failure(stderr: &str) -> bool {
    const NEEDLES: &[&str] = &[
        // libcurl transport faults surfaced verbatim by cargo's HTTP stack.
        "error in the http2 framing layer", // the v0.11.3 makeself failure
        "connection reset by peer",
        "connection refused",
        "could not resolve host",
        "couldn't resolve host",
        "resolving timed out",
        "operation timed out",
        "transfer closed",
        // cargo's own transport-layer wording. Deliberately NOT the bare
        // "failed to download": cargo emits it for a non-transient missing/
        // yanked dependency too (paired with "no matching package", which the
        // propagation class already handles), so matching it here would burn
        // retries on a hard resolution error. The curl faults above and the
        // request-send phrases below cover genuine transport download failures.
        "spurious network error",
        "failed to get successful http response",
        "failed to send http request",
        "error sending request",
        // 5xx / rate-limit from the registry or its CDN edge.
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "429 too many requests",
    ];
    let lower = stderr.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Run `cargo publish` with bounded retry on the two recoverable failure
/// classes: sparse-index propagation lag ([`is_index_propagation_failure`])
/// and transient network/transport faults ([`is_transient_network_failure`]).
///
/// This is defense-in-depth on top of [`poll_crates_io_index`] and cargo's
/// own internal transport retries. Even after our wait sees the just-published
/// dep on the crates.io sparse index, the dependent crate's own `cargo publish`
/// may race against Fastly's inter-edge fan-out and land on a stale edge; and a
/// momentary TCP/TLS/HTTP blip can exhaust cargo's bounded internal retries
/// mid-publish (the v0.11.3 `HTTP2 framing layer` abort). Retrying exclusively
/// on those two narrow signature sets recovers both windows without masking
/// real failures (auth, packaging, validation) — which match neither
/// discriminator and fast-fail on the first attempt.
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
        let propagation = is_index_propagation_failure(&stderr);
        let transient_net = is_transient_network_failure(&stderr);
        if !propagation && !transient_net {
            // Neither recoverable class — surface immediately. check_output
            // performs redaction + error formatting consistently with the
            // single-attempt path.
            return log.check_output(output, label);
        }

        // Name which recoverable class fired so the operator can tell an
        // edge-skew retry from a network-blip retry in the run log.
        let kind = if propagation {
            "sparse-index propagation lag"
        } else {
            "transient network error"
        };

        if attempt >= PUBLISH_PROPAGATION_RETRIES {
            log.warn(&format!(
                "{kind} for {label} persists after {attempt} attempts; surfacing"
            ));
            last_output = Some(output);
            break;
        }

        log.status(&format!(
            "{kind} detected for {label} (attempt {}/{}); retrying in {}s",
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
    let all_crates: Vec<CrateConfig> = ctx.config.crate_universe().into_iter().cloned().collect();

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
                        "skipped cargo publish for '{}' — skip=true",
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
                    "skipped cargo publish for '{}' — `if` condition evaluated falsy",
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

/// Resolve the project-wide `default_targets` the build stage would use:
/// `defaults.targets` when non-empty, else the canonical default matrix.
///
/// Routed through `Config::effective_default_targets` — the same helper the
/// build stage uses — so the binstall override set the cargo publisher emits
/// equals the one the build stage emits for the same config; any divergence
/// would surface as a per-target asset mismatch between the two paths.
fn resolve_default_targets(ctx: &Context) -> Vec<String> {
    ctx.config.effective_default_targets()
}

/// Guarantee `[package.metadata.binstall]` is present and current in
/// `crate_cfg`'s on-disk `Cargo.toml` immediately before `cargo publish`.
///
/// Binstall metadata is a *published-manifest* property: `cargo binstall`
/// reads it from the manifest on crates.io to fetch a prebuilt asset instead
/// of compiling from source. The build stage emits it too, but the real
/// release runs `anodizer release --publish-only`, which consumes preserved
/// dist artifacts and skips the build stage entirely — so without this call the
/// published manifest carries no binstall metadata and `cargo binstall`
/// silently falls back to a source compile.
///
/// The emitter is idempotent (it re-writes only the keys it owns and preserves
/// user-authored ones), so invoking it here when the build stage already ran in
/// the full pipeline is a safe no-op-equivalent rewrite, not a double-write
/// divergence. Per-crate template vars are re-scoped via [`with_crate_scope`]
/// exactly as the build stage does, so the emitted overrides are byte-identical
/// across the two paths in single-crate, workspace-lockstep, and workspace
/// per-crate modes.
///
/// Honors `dry_run` (the emitter does not mutate under dry-run); the caller
/// already early-returns before the publish loop on `ctx.is_dry_run()`, so in
/// practice this only runs on a real publish.
/// The per-crate tag source is injected. The publish loop passes
/// [`anodizer_core::crate_scope::resolve_crate_tag`] (git-backed, threaded in
/// from the public entry points); tests pass a closure returning a fixed tag so
/// the per-crate var scoping — and the resulting override set — can be exercised
/// without a git fixture. Mirrors the build stage's
/// `apply_source_mutations_with_resolver`
/// seam so both paths are testable the same way.
fn ensure_binstall_metadata_with(
    ctx: &mut Context,
    crate_cfg: &CrateConfig,
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    let Some(ref binstall_cfg) = crate_cfg.binstall else {
        return Ok(());
    };
    if !binstall_cfg.enabled.unwrap_or(false) {
        return Ok(());
    }
    let default_targets = resolve_default_targets(ctx);
    let binstall_cfg = binstall_cfg.clone();
    anodizer_core::crate_scope::with_crate_scope(ctx, crate_cfg, resolve_tag, |ctx| {
        anodizer_core::binstall::generate_binstall_metadata(
            crate_cfg,
            &binstall_cfg,
            &default_targets,
            ctx,
            dry_run,
        )
    })
    .with_context(|| {
        format!(
            "publish: ensure binstall metadata for '{}' before cargo publish",
            crate_cfg.name
        )
    })?;
    log.verbose(&format!(
        "ensured [package.metadata.binstall] in {}/Cargo.toml before publish",
        crate_cfg.path
    ));
    Ok(())
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

/// Test seam for [`publish_to_cargo`] that injects only the crates.io
/// already-published index check; the content-vs-version guard's local
/// `.crate` checksum is wired to the production [`local_crate_cksum`].
///
/// Production passes [`is_already_published`] (a real sparse-index GET);
/// tests pass a stub so the partial-failure rollback path can be exercised
/// without a network round-trip. The signature mirrors `is_already_published`
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
        &StageLogger,
    ) -> Result<Option<String>>,
) -> Result<()> {
    publish_to_cargo_with_guard(
        ctx,
        selected,
        log,
        record,
        already_published_check,
        |name, crate_cfg, cargo_cfg| local_crate_cksum(name, crate_cfg, cargo_cfg, log),
        &anodizer_core::crate_scope::resolve_crate_tag,
        fetch_published_crate,
    )
}

/// Full test seam: both the crates.io already-published index check AND the
/// content-vs-version guard's local `.crate` checksum computer are injected.
///
/// The local-cksum stub returns `(crate_name, crate_cfg, cargo_cfg) ->
/// Result<Option<LocalCrate>>`:
/// - `Ok(Some(LocalCrate))` — the local `.crate` sha256 + bytes the guard
///   compares against the index-recorded `cksum` (fast path) and, on
///   mismatch, against the fetched published `.crate` (slow path).
/// - `Ok(None)` — guard inapplicable (non-crates.io registry); the
///   already-published skip is also suppressed for that crate.
/// - `Err(_)` — local digest uncomputable; the guard FAILS CLOSED rather
///   than treat an unverifiable already-published version as a safe skip.
///
/// `fetch_published` mirrors `already_published_check`'s injection pattern —
/// production wires [`fetch_published_crate`] (a real crates.io static-CDN
/// GET), tests inject a stub so the slow path (only reached when the local
/// and index cksums disagree) can be exercised without a network round-trip.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn publish_to_cargo_with_guard(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
    record: &mut Vec<CargoYankTarget>,
    already_published_check: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
        &StageLogger,
    ) -> Result<Option<String>>,
    local_cksum_check: impl Fn(
        &str,
        &CrateConfig,
        Option<&CargoPublishConfig>,
    ) -> Result<Option<LocalCrate>>,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
    fetch_published: impl Fn(
        &str,
        &str,
        &anodizer_core::retry::RetryPolicy,
        &StageLogger,
    ) -> Result<Vec<u8>>,
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
            log.verbose(&run_per_crate_start_message(name));
            let cmd = publish_command(name, cargo_cfgs.get(name));
            log.status(&format!("(dry-run) would run: {}", cmd.join(" ")));
            // Surface that the content-vs-version poison guard would run for
            // any crate already on crates.io — operators see WHAT would be
            // checked without a network round-trip or local package step.
            if targets_crates_io(cargo_cfgs.get(name)) {
                log.status(&format!(
                    "(dry-run) would verify '{}' local .crate checksum against the crates.io index if already published",
                    name
                ));
            }
        }
        return Ok(());
    }

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every crate's index-check GET. Mirrors the per-pipe-invocation
    // pattern used by artifactory/cloudsmith.
    let retry_policy = ctx.retry_policy();

    // Resolved once for the whole publish set: whether this run's config has
    // the changelog stage regenerating on-disk CHANGELOG.md files, which is
    // what lets the already-published guard treat crate-root CHANGELOG.md
    // drift on a re-cut as anodizer's own artifact instead of a poison.
    let changelog_stage_active = changelog_stage_regenerates_files(ctx);

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
        let probe = |name: &str, version: &str| match already_published_check(
            name,
            version,
            &retry_policy,
            log,
        ) {
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

    // Working-tree cleanliness gate — ONCE, before the loop's first binstall
    // write dirties the tree. Checked here (not per crate) because the binstall
    // mutation for crate A would otherwise dirty the tree and false-trip the
    // check for crate B in a multi-crate workspace. A dirty tree at entry means
    // `cargo package` stamps `"dirty": true` into `.cargo_vcs_info.json`,
    // changing the `.crate` bytes vs the clean tag checkout the original release
    // published from — so the content-vs-index comparison is unreliable (false
    // poison on a clean-published crate, or masking real drift). Fail loud
    // rather than skip (a poison hole) or hard-fail on content (which would
    // misattribute the divergence to a code change). Only gates when at least
    // one crate in the set could actually run the guard (crates.io target with a
    // resolved version); a pure non-crates.io / unversioned set never packages
    // for comparison, so a dirty tree there is irrelevant.
    let any_guarded = sorted_names.iter().any(|name| {
        targets_crates_io(cargo_cfgs.get(name))
            && !crate_versions
                .get(name)
                .cloned()
                .unwrap_or_default()
                .is_empty()
    });
    if any_guarded {
        ensure_publish_tree_clean(ctx)?;
    }

    for (i, name) in sorted_names.iter().enumerate() {
        log.verbose(&run_per_crate_start_message(name));
        // Per-crate resolved version (own Cargo.toml `[package].version`,
        // falling back to the release version) — sourced from the plan so the
        // already-published check uses the same version the preflight queried.
        let crate_version = crate_versions.get(name).cloned().unwrap_or_default();

        let cargo_cfg = cargo_cfgs.get(name);
        let crate_cfg = all_crates.iter().find(|c| &c.name == name);

        // binstall metadata BEFORE the skip-decision packages — so
        // `local_crate_cksum` hashes the SAME on-disk tree `cargo publish`
        // uploads. The original publish wrote this table, so the crates.io
        // cksum reflects it; packaging without it would mismatch and
        // false-poison every binstall crate's clean re-cut (anodizer's own
        // `cli` crate carries `binstall.enabled: true`). Mutating in place is
        // byte-identical-by-construction: the real publish mutates the same tree
        // and never reverts it, so there is no second tree to keep in sync. The
        // tree was verified clean once before the loop, so this is the only
        // dirtiness `cargo package` will see — matching the original publish.
        if let Some(crate_cfg) = crate_cfg {
            ensure_binstall_metadata_with(ctx, crate_cfg, false, log, resolve_tag)?;
        }

        // Idempotency + poison guard: if this version already exists on
        // crates.io, the publish may be a safe re-cut (byte-identical content)
        // or a SILENT POISON (content changed but the version was not bumped —
        // `cargo publish` would skip, never shipping the new bytes, while
        // anodizer reports success and consumers get stale code). Before
        // treating an already-published version as a safe skip, prove the
        // local `.crate` is byte-identical to the published artifact by
        // comparing sha256 against the index-recorded `cksum`. The local
        // package step now reflects the binstall mutation applied above, so the
        // hash matches what the original `cargo publish` uploaded.
        //
        // The skip — and the guard — apply ONLY to crates.io targets: a custom
        // `registry =`/`index =` points cargo at a different index, so the
        // crates.io cksum describes a different (or no) artifact. For those,
        // attempt publish and let the target registry's server-side conflict
        // handling govern idempotency.
        //
        // Index check failures (network) FAIL CLOSED for an already-published
        // decision: an unreachable index cannot prove the version is absent,
        // and silently skipping a maybe-poisoned version is the bug this guard
        // exists to prevent.
        let guard = if crate_version.is_empty() || !targets_crates_io(cargo_cfg) {
            CargoSkipDecision::Publish
        } else {
            match already_published_check(name, &crate_version, &retry_policy, log) {
                Ok(None) => CargoSkipDecision::Publish,
                Ok(Some(index_cksum)) => {
                    let crate_cfg = crate_cfg.ok_or_else(|| {
                        anyhow::anyhow!(
                            "publish: '{name}-{crate_version}' is published on crates.io but its \
                             crate config is missing; cannot verify content identity"
                        )
                    })?;
                    decide_already_published(
                        name,
                        &crate_version,
                        &index_cksum,
                        crate_cfg,
                        cargo_cfg,
                        changelog_stage_active,
                        &local_cksum_check,
                        |n, v| fetch_published(n, v, &retry_policy, log),
                        log,
                    )?
                }
                Err(e) => {
                    // Fail closed: do not silently skip a version we cannot
                    // confirm is byte-identical to what shipped.
                    anyhow::bail!(
                        "publish: could not reach the crates.io index to verify '{name}-{crate_version}' \
                         is safe to skip ({e}); refusing to skip a possibly-poisoned already-published \
                         version. Resolve the network issue and re-run, or bump the version."
                    );
                }
            }
        };
        if matches!(guard, CargoSkipDecision::Skip) {
            log.status(&format!(
                "skipped '{}-{}' — already published on crates.io with verified equivalent \
                 content",
                name, crate_version
            ));
            continue;
        }

        // (binstall metadata was emitted above, before the skip-decision, so
        // the local package step the guard ran reflects the same on-disk tree
        // `cargo publish` is about to upload. It is needed regardless of the
        // skip outcome — the real release runs `--publish-only`, which skips
        // the build stage, so the table must exist before publish either way.)

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
        log.verbose(&format!("running {}", cmd.join(" ")));

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
                    "skipped index poll for '{}' — index_timeout is 0 (dependents may fail)",
                    name
                ));
            } else {
                log.verbose(&format!(
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
/// The crate-level `publish.cargo` block — the single accessor the
/// registry gate and the gate-override collapse key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::CargoPublishConfig> {
    p.cargo.as_ref()
}

simple_publisher!(
    CargoPublisher,
    "cargo",
    anodizer_core::PublisherGroup::Submitter,
    true,
    Some("CARGO_REGISTRY_TOKEN yank"),
);

/// Operator-visible start line for the cargo publisher. Worded apart from
/// [`crate::publisher_helpers::run_start_message`] on purpose: cargo
/// processes every selected crate rather than scanning them for a config
/// block, so "scanning … for a cargo config block" would misdescribe it.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "starting cargo publish — processing {} selected crate(s)",
        selected_total
    )
}

/// Operator-visible per-crate start line. Emitted by `publish_to_cargo`
/// immediately before each crate's publish-or-skip decision so the
/// per-crate progress is anchored to a specific name in the log.
/// Mirrors `run_per_crate_start_message` on every other per-crate
/// publisher (homebrew, scoop, nix, aur, krew).
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate cargo publish for '{}'", crate_name)
}

/// Operator-visible done line, emitted after `publish_to_cargo` returns
/// Ok. `processed` counts crates whose publish path was actually
/// invoked (skipped-by-already-published, skipped-by-skip-template, and
/// dry-run paths all count as processed — they're successful runs of
/// the correct code path).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished cargo publish — {} selected crate(s) processed",
        processed
    )
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.cargo` block) but `publish_to_cargo` resolved
/// zero publishable crates (every cargo-configured crate was filtered
/// out by `--crate` / `--all` selection).
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "cargo publisher registered but 0 of {} effective crate(s) had a cargo \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.cargo block is set.",
        selected_total
    )
}

/// Count of crates in the crate universe (after `--crate` / `--all`
/// selection) carrying a `publish.cargo` block. Used by the publisher's run
/// wrapper to choose between the `done` and `no-eligible-crates` log paths.
fn count_cargo_configured_crates(ctx: &Context) -> usize {
    let all = ctx.config.crate_universe();
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
        let configured = ctx
            .config
            .crate_universe()
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
        // `skipped ... already published`) plus the per-crate-start line
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

    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Token VALIDITY only — duplicate-version and partial-publish are
        // already caught by the state-query checker + `cargo publish
        // --dry-run`. `requirements()` gates token PRESENCE; this proves the
        // present token is accepted before the irreversible first publish.
        // Only probe crates.io when an ACTIVE cargo publisher targets the
        // default registry. An entry with `registry:`/`index:` set publishes
        // to a private registry whose credential is `CARGO_REGISTRIES_<NAME>_TOKEN`,
        // NOT the `CARGO_REGISTRY_TOKEN` this probe presents to
        // `crates.io/api/v1/me` — probing crates.io for it would false-Blocker a
        // perfectly valid private-registry release. Holds across single-crate,
        // lockstep, and per-crate modes (per-crate entries may each pick a
        // different registry).
        let probes_crates_io = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.cargo.as_ref())
            .filter(|cargo| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    cargo.skip.as_ref(),
                    None,
                    cargo.if_condition.as_deref(),
                )
            })
            .any(|cargo| cargo.registry.is_none() && cargo.index.is_none());
        if !probes_crates_io {
            return Ok(anodizer_core::PreflightCheck::Pass);
        }
        let token = ctx
            .env_source()
            .var("CARGO_REGISTRY_TOKEN")
            .unwrap_or_default();
        if token.is_empty() {
            return Ok(anodizer_core::PreflightCheck::Pass);
        }
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let api_url = format!("{}/api/v1/me", crates_io_api_base());
        Ok(
            match crate::publisher_preflight::probe_token_auth(
                &api_url,
                &token,
                "preflight: crates.io token",
                &policy,
                &ctx.logger("preflight"),
            ) {
                crate::publisher_preflight::TokenAuth::Valid => anodizer_core::PreflightCheck::Pass,
                crate::publisher_preflight::TokenAuth::Invalid => {
                    anodizer_core::PreflightCheck::Blocker("crates.io token invalid".into())
                }
                crate::publisher_preflight::TokenAuth::Indeterminate(reason) => {
                    anodizer_core::git::indeterminate_check(
                        ctx.preflight_is_strict(),
                        format!("could not verify crates.io token ({reason})"),
                    )
                }
            },
        )
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
    let universe = ctx.config.crate_universe();
    let name = universe
        .iter()
        .copied()
        .find(|c| !project_name.is_empty() && c.name == project_name && eligible(c))
        .or_else(|| universe.iter().copied().find(eligible))
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
        assert!(msg.starts_with("starting cargo publish"), "{msg}");
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
        assert!(msg.contains("2 selected crate(s) processed"), "{msg}");
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
    fn cargo_preflight_passes_when_unconfigured() {
        // No `publish.cargo` block ⇒ the token-validity probe is skipped
        // (nothing to publish), so no network round-trip occurs. The live
        // 401⇒Blocker / 2xx⇒Pass mapping is covered by
        // `publisher_preflight::tests::token_auth_*`.
        let ctx = TestContextBuilder::new().build();
        let p = CargoPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn cargo_preflight_skips_crates_io_probe_for_alternate_registry() {
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

        // A non-default registry publishes with `CARGO_REGISTRIES_<NAME>_TOKEN`,
        // NOT the crates.io `CARGO_REGISTRY_TOKEN` this probe presents. Even
        // with a token present, the crates.io `/me` probe must be skipped
        // (returns Pass without a network hit) so a private-registry release is
        // never false-Blockered.
        let crate_cfg = CrateConfig {
            name: "mytool".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig {
                    registry: Some("my-corp".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .project_name("mytool")
            .crates(vec![crate_cfg])
            .env("CARGO_REGISTRY_TOKEN", "present-but-for-another-registry")
            .build();
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

    // ---- content-vs-version guard decision unit tests --------------------

    /// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
    /// given `(in-tar path, content)` entries — for `crates_equal_modulo_vcs`
    /// and `decide_already_published` fixtures that need real archive bytes
    /// rather than opaque cksum labels.
    fn make_crate_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;

        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *content)
                .expect("append tar entry");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    /// Like [`make_crate_tarball`] but writes each entry's path directly into
    /// the header's raw name bytes instead of going through
    /// `tar::Builder::append_data`. `append_data` normalizes the path via
    /// `tar`'s `copy_path_into_inner`, which deliberately drops a leading
    /// `./` (`Component::CurDir`) — so it can't produce the `./`-prefixed
    /// root entry the leading-CurDir hardening test below needs to prove
    /// against. `tar::Builder::append` (unlike `append_data`) writes the
    /// header as-is with no path processing.
    fn make_crate_tarball_raw_paths(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;

        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            let path_bytes = path.as_bytes();
            let name_slot = &mut header.as_old_mut().name;
            assert!(
                path_bytes.len() < name_slot.len(),
                "raw path fixture '{path}' too long for the tar header name field"
            );
            name_slot[..path_bytes.len()].copy_from_slice(path_bytes);
            header.set_cksum();
            builder
                .append(&header, *content)
                .expect("append raw tar entry");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    /// Minimal `.cargo_vcs_info.json` body: `{"git":{"sha1":"<sha>"},"path_in_vcs":"<vcs_path>"}`.
    fn vcs_info_json(sha1: &str, path_in_vcs: &str) -> Vec<u8> {
        format!(r#"{{"git":{{"sha1":"{sha1}"}},"path_in_vcs":"{path_in_vcs}"}}"#).into_bytes()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
    }

    #[test]
    fn crates_equal_modulo_vcs_identical_archives_match() {
        let bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("deadbeef", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&bytes, &bytes, false).expect("compare");
        assert!(matches!(m, CrateContentMatch::Equivalent { .. }));
    }

    #[test]
    fn crates_equal_modulo_vcs_differs_only_in_vcs_sha1_matches() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_b", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        assert!(
            matches!(m, CrateContentMatch::Equivalent { .. }),
            "a git.sha1-only delta is a same-source re-cut"
        );
    }

    #[test]
    fn crates_equal_modulo_vcs_differs_in_src_file_reports_path() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/src/lib.rs", b"fn a() { /* changed */ }"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/src/lib.rs".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => panic!("a real source edit must be flagged"),
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_differs_in_vcs_non_sha_field_reports_path() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "subdir"),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/.cargo_vcs_info.json".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("a path_in_vcs change is structural drift, not just the commit stamp")
            }
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_extra_entry_reports_path() {
        let local = make_crate_tarball(&[("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n")]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            ("c-1.0.0/src/extra.rs", b"// only in published"),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/src/extra.rs".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("an entry present in only one archive must be flagged")
            }
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_nested_decoy_is_byte_compared() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
            (
                "c-1.0.0/tests/data/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
            (
                "c-1.0.0/tests/data/.cargo_vcs_info.json",
                &vcs_info_json("commit_b", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(
                    paths,
                    vec!["c-1.0.0/tests/data/.cargo_vcs_info.json".to_string()]
                );
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("a nested .cargo_vcs_info.json is ordinary source, not the root vcs stamp")
            }
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_root_vcs_info_still_normalized() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_b", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        assert!(
            matches!(m, CrateContentMatch::Equivalent { .. }),
            "the root .cargo_vcs_info.json's git.sha1 is still normalized"
        );
    }

    #[test]
    fn crates_equal_modulo_vcs_root_vcs_info_dot_slash_prefixed_still_normalized() {
        let local = make_crate_tarball_raw_paths(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "./c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball_raw_paths(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "./c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_b", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        assert!(
            matches!(m, CrateContentMatch::Equivalent { .. }),
            "a leading `./` (a CurDir component) must not inflate the root gate's \
             component count and misclassify the crate-root vcs-info as nested source"
        );
    }

    #[test]
    fn crates_equal_modulo_vcs_root_vcs_info_missing_on_one_side_differs() {
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published = make_crate_tarball(&[("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n")]);
        let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/.cargo_vcs_info.json".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("a root vcs-info present on only one side is an unambiguous divergence")
            }
        }
    }

    #[test]
    fn targets_crates_io_true_for_default_and_false_for_custom() {
        assert!(targets_crates_io(None), "no cfg ⇒ crates.io");
        assert!(
            targets_crates_io(Some(&CargoPublishConfig::default())),
            "empty cfg ⇒ crates.io"
        );
        let custom_reg = CargoPublishConfig {
            registry: Some("corp".into()),
            ..Default::default()
        };
        assert!(!targets_crates_io(Some(&custom_reg)), "registry= ⇒ custom");
        let custom_idx = CargoPublishConfig {
            index: Some("https://example/index".into()),
            ..Default::default()
        };
        assert!(!targets_crates_io(Some(&custom_idx)), "index= ⇒ custom");
    }

    /// Fetch closure that panics if invoked — for tests proving a code path
    /// never reaches the slow-path download.
    fn fetch_panics(_: &str, _: &str) -> Result<Vec<u8>> {
        panic!("fetch_published must not run on this path")
    }

    #[test]
    fn decide_already_published_empty_index_cksum_fails_closed() {
        // An empty cksum on a returned index entry cannot prove content
        // identity. Skipping it would reopen the poison hole, so the guard
        // fails closed WITHOUT invoking the local computer or the fetcher.
        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_panics = |_: &str,
                            _: &CrateConfig,
                            _: Option<&CargoPublishConfig>|
         -> Result<Option<LocalCrate>> {
            panic!("local cksum must not run when index cksum is empty")
        };
        let err = decide_already_published(
            "c",
            "1.0.0",
            "",
            &cfg,
            None,
            false,
            local_panics,
            fetch_panics,
            &log,
        )
        .expect_err("empty cksum ⇒ fail closed, never skip");
        assert!(
            err.to_string().contains("carries no cksum"),
            "actionable empty-cksum error: {err}"
        );
    }

    #[test]
    fn decide_already_published_local_none_fails_closed() {
        // Ok(None) means no local digest for a crates.io-targeting crate — an
        // unverifiable state the main loop should never reach, so the guard
        // refuses to skip rather than silently pass a possibly-drifted version.
        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_none = |_: &str,
                          _: &CrateConfig,
                          _: Option<&CargoPublishConfig>|
         -> Result<Option<LocalCrate>> { Ok(None) };
        let err = decide_already_published(
            "c",
            "1.0.0",
            "abcd",
            &cfg,
            None,
            false,
            local_none,
            fetch_panics,
            &log,
        )
        .expect_err("local None ⇒ fail closed, never skip");
        assert!(
            err.to_string().contains("content identity is unverifiable"),
            "actionable local-None error: {err}"
        );
    }

    #[test]
    fn decide_already_published_match_is_case_insensitive_skip() {
        // Fast path: local sha256 == index cksum (case-insensitive) ⇒ Skip
        // WITHOUT ever invoking the (panicking) fetch closure.
        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "ABCD".to_string(),
                bytes: Vec::new(),
            }))
        };
        let d = decide_already_published(
            "c",
            "1.0.0",
            "abcd",
            &cfg,
            None,
            false,
            local,
            fetch_panics,
            &log,
        )
        .expect("case-insensitive match ⇒ Skip, no download");
        assert_eq!(d, CargoSkipDecision::Skip);
    }

    #[test]
    fn decide_already_published_slow_path_identical_modulo_vcs_skips() {
        // Local sha256 != index cksum (the fast path misses), but the
        // fetched published .crate is identical to the local one except for
        // .cargo_vcs_info.json's git.sha1 — the same-source-re-cut case the
        // whole slow path exists for.
        let local_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_new", "."),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_old", "."),
            ),
        ]);
        let index_cksum = sha256_hex(&published_bytes);
        let local_cksum = sha256_hex(&local_bytes);
        assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_bytes_clone = local_bytes.clone();
        let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: local_cksum.clone(),
                bytes: local_bytes_clone.clone(),
            }))
        };
        let published_bytes_clone = published_bytes.clone();
        let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
        let d = decide_already_published(
            "c",
            "1.0.0",
            &index_cksum,
            &cfg,
            None,
            false,
            local,
            fetch,
            &log,
        )
        .expect("same-source re-cut (vcs-only delta) ⇒ Skip");
        assert_eq!(d, CargoSkipDecision::Skip);
    }

    #[test]
    fn decide_already_published_slow_path_real_drift_hard_fails() {
        // Local sha256 != index cksum, and the fetched published .crate has a
        // GENUINE content difference (not just the vcs stamp) ⇒ hard fail,
        // naming the differing path.
        let local_bytes = make_crate_tarball(&[
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("c-1.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a", "."),
            ),
        ]);
        let index_cksum = sha256_hex(&published_bytes);
        let local_cksum = sha256_hex(&local_bytes);
        assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_bytes_clone = local_bytes.clone();
        let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: local_cksum.clone(),
                bytes: local_bytes_clone.clone(),
            }))
        };
        let published_bytes_clone = published_bytes.clone();
        let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
        let err = decide_already_published(
            "c",
            "1.0.0",
            &index_cksum,
            &cfg,
            None,
            false,
            local,
            fetch,
            &log,
        )
        .expect_err("real content drift ⇒ hard fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("DIFFERENT content"), "{msg}");
        assert!(
            msg.contains("c-1.0.0/src/lib.rs"),
            "error must name the differing path: {msg}"
        );
    }

    #[test]
    fn decide_already_published_published_fetch_err_fails_closed() {
        // The fast path misses; fetching the published .crate to run the
        // slow-path comparison fails (network) ⇒ fail closed, never skip a
        // version whose content identity couldn't be confirmed either way.
        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "local_sha".to_string(),
                bytes: Vec::new(),
            }))
        };
        let fetch_err =
            |_: &str, _: &str| -> Result<Vec<u8>> { Err(anyhow::anyhow!("connection refused")) };
        let err = decide_already_published(
            "c",
            "1.0.0",
            "index_sha",
            &cfg,
            None,
            false,
            local,
            fetch_err,
            &log,
        )
        .expect_err("published fetch failure ⇒ fail closed");
        assert!(
            format!("{err:#}").contains("could not be fetched"),
            "{err:#}"
        );
    }

    #[test]
    fn decide_already_published_published_sha_mismatch_fails_closed() {
        // The fast path misses; the fetched "published" bytes don't actually
        // hash to the index cksum — a mismatched download is not a valid
        // comparison basis ⇒ fail closed rather than trust it either way.
        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "local_sha".to_string(),
                bytes: Vec::new(),
            }))
        };
        let fetch = |_: &str, _: &str| Ok(b"not the real published bytes".to_vec());
        let err = decide_already_published(
            "c",
            "1.0.0",
            "index_sha_that_wont_match",
            &cfg,
            None,
            false,
            local,
            fetch,
            &log,
        )
        .expect_err("published-sha mismatch ⇒ fail closed");
        assert!(format!("{err:#}").contains("does NOT match"));
    }

    /// Normalized lib-only packaged manifest, as `cargo package` writes it
    /// (explicit `[lib]`, no `[[bin]]`).
    const LIB_ONLY_MANIFEST: &[u8] =
        b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[lib]\npath = \"src/lib.rs\"\n";

    /// Normalized packaged manifest carrying an explicit `[[bin]]` target.
    const BIN_MANIFEST: &[u8] = b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[[bin]]\nname = \"c\"\npath = \"src/main.rs\"\n";

    #[test]
    fn decide_already_published_recut_changelog_and_lockfile_skips_with_changelog_stage() {
        // The exact cfgd-crd@0.5.0 scenario: a re-cut of a partially-published
        // workspace release where the published crate and the local re-cut
        // differ in exactly two crate-root files — CHANGELOG.md (regenerated
        // by anodizer's changelog stage between re-cuts) and Cargo.lock (the
        // workspace lockfile moved via an unrelated dependency bump) — on a
        // lib-only crate. Sources identical ⇒ safe idempotent Skip.
        let local_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            (
                "c-1.0.0/CHANGELOG.md",
                b"# Changelog\n\n## 1.0.0 (re-cut)\n",
            ),
            ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_new", "."),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/CHANGELOG.md", b"# Changelog\n\n## 1.0.0\n"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_old", "."),
            ),
        ]);
        let index_cksum = sha256_hex(&published_bytes);
        let local_cksum = sha256_hex(&local_bytes);
        assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_bytes_clone = local_bytes.clone();
        let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: local_cksum.clone(),
                bytes: local_bytes_clone.clone(),
            }))
        };
        let published_bytes_clone = published_bytes.clone();
        let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
        let d = decide_already_published(
            "c",
            "1.0.0",
            &index_cksum,
            &cfg,
            None,
            true,
            local,
            fetch,
            &log,
        )
        .expect("changelog+lockfile-only re-cut of a lib crate ⇒ Skip");
        assert_eq!(d, CargoSkipDecision::Skip);
    }

    #[test]
    fn decide_already_published_changelog_drift_without_changelog_stage_hard_fails() {
        // Same CHANGELOG.md delta, but no changelog stage configured for the
        // run: anodizer did not regenerate the file, so the drift is real and
        // the guard must hard-fail, naming the file AND the why.
        let local_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            (
                "c-1.0.0/CHANGELOG.md",
                b"# Changelog\n\n## 1.0.0 (edited)\n",
            ),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_new", "."),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/CHANGELOG.md", b"# Changelog\n\n## 1.0.0\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_old", "."),
            ),
        ]);
        let index_cksum = sha256_hex(&published_bytes);
        let local_cksum = sha256_hex(&local_bytes);

        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_bytes_clone = local_bytes.clone();
        let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: local_cksum.clone(),
                bytes: local_bytes_clone.clone(),
            }))
        };
        let published_bytes_clone = published_bytes.clone();
        let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
        let err = decide_already_published(
            "c",
            "1.0.0",
            &index_cksum,
            &cfg,
            None,
            false,
            local,
            fetch,
            &log,
        )
        .expect_err("CHANGELOG.md drift with no changelog stage ⇒ hard fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("c-1.0.0/CHANGELOG.md"),
            "must name the file: {msg}"
        );
        assert!(
            msg.contains("no changelog stage is configured/enabled for this run"),
            "must say why the file was not treated as equivalent: {msg}"
        );
    }

    #[test]
    fn decide_already_published_lockfile_drift_on_binary_crate_hard_fails() {
        // Root Cargo.lock delta on a crate WITH a [[bin]] target: the packaged
        // lockfile is consumer-visible via `cargo install --locked`, so it
        // stays byte-strict — hard fail naming the file AND the why.
        let local_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
            ("c-1.0.0/src/main.rs", b"fn main() {}"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_new", "."),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
            ("c-1.0.0/src/main.rs", b"fn main() {}"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_old", "."),
            ),
        ]);
        let index_cksum = sha256_hex(&published_bytes);
        let local_cksum = sha256_hex(&local_bytes);

        let cfg = CrateConfig::default();
        let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
        let local_bytes_clone = local_bytes.clone();
        let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: local_cksum.clone(),
                bytes: local_bytes_clone.clone(),
            }))
        };
        let published_bytes_clone = published_bytes.clone();
        let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
        let err = decide_already_published(
            "c",
            "1.0.0",
            &index_cksum,
            &cfg,
            None,
            true,
            local,
            fetch,
            &log,
        )
        .expect_err("Cargo.lock drift on a binary crate ⇒ hard fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("c-1.0.0/Cargo.lock"),
            "must name the file: {msg}"
        );
        assert!(
            msg.contains("binary or example targets") && msg.contains("cargo install --locked"),
            "must say why the lockfile stayed byte-strict: {msg}"
        );
    }

    #[test]
    fn crates_equal_modulo_vcs_source_drift_beside_normalizable_files_differs() {
        // A real src/lib.rs edit rides along with the forgivable metadata
        // deltas: the source drift must still be flagged — the normalizations
        // never mask a genuine content change.
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/CHANGELOG.md", b"# Changelog (re-cut)\n"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_new", "."),
            ),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
            ("c-1.0.0/CHANGELOG.md", b"# Changelog\n"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
            (
                "c-1.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_old", "."),
            ),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/src/lib.rs".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("a real source edit must be flagged even beside forgivable metadata")
            }
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_nested_changelog_and_lockfile_are_byte_compared() {
        // Root-only discipline: CHANGELOG.md / Cargo.lock at 3+ Normal
        // components are ordinary packaged source (e.g. test fixtures) and
        // must be byte-compared, never normalized.
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/tests/data/CHANGELOG.md", b"fixture a"),
            ("c-1.0.0/tests/data/Cargo.lock", b"fixture lock a"),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/tests/data/CHANGELOG.md", b"fixture b"),
            ("c-1.0.0/tests/data/Cargo.lock", b"fixture lock b"),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(
                    paths,
                    vec![
                        "c-1.0.0/tests/data/CHANGELOG.md".to_string(),
                        "c-1.0.0/tests/data/Cargo.lock".to_string(),
                    ]
                );
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("nested CHANGELOG.md / Cargo.lock are ordinary source, never normalized")
            }
        }
    }

    #[test]
    fn crates_equal_modulo_vcs_cargo_toml_drift_always_differs() {
        // Cargo.toml is NEVER in the equivalence set — a manifest delta is
        // real drift regardless of the changelog-stage flag.
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ]);
        let published = make_crate_tarball(&[
            (
                "c-1.0.0/Cargo.toml",
                b"[package]\nname = \"c\"\nversion = \"1.0.0\"\nedition = \"2024\"\n".as_slice(),
            ),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ]);
        let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
        match m {
            CrateContentMatch::Differs(paths) => {
                assert_eq!(paths, vec!["c-1.0.0/Cargo.toml".to_string()]);
            }
            CrateContentMatch::Equivalent { .. } => {
                panic!("a Cargo.toml delta must always be flagged")
            }
        }
    }

    #[test]
    fn packaged_crate_has_bin_targets_reads_the_normalized_manifest() {
        let lib_only = read_crate_entries(&make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ]))
        .expect("unpack");
        assert_eq!(packaged_crate_has_bin_targets(&lib_only), Some(false));

        let with_bin = read_crate_entries(&make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
            ("c-1.0.0/src/main.rs", b"fn main() {}"),
        ]))
        .expect("unpack");
        assert_eq!(packaged_crate_has_bin_targets(&with_bin), Some(true));

        // Conventional bin sources count even without an explicit [[bin]]
        // (belt-and-braces against implicit target auto-discovery).
        let implicit_bin = read_crate_entries(&make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/main.rs", b"fn main() {}"),
        ]))
        .expect("unpack");
        assert_eq!(packaged_crate_has_bin_targets(&implicit_bin), Some(true));

        // No root Cargo.toml ⇒ indeterminate ⇒ caller fails closed.
        let no_manifest = read_crate_entries(&make_crate_tarball(&[(
            "c-1.0.0/src/lib.rs",
            b"fn a() {}".as_slice(),
        )]))
        .expect("unpack");
        assert_eq!(packaged_crate_has_bin_targets(&no_manifest), None);
    }

    #[test]
    fn packaged_crate_examples_count_as_installable_targets() {
        // `cargo install --example` consumes the packaged lockfile just like
        // a bin install, so an explicit [[example]] disqualifies the crate
        // from the lib-only Cargo.lock forgiveness.
        const EXAMPLE_MANIFEST: &[u8] = b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[lib]\nname = \"c\"\npath = \"src/lib.rs\"\n\n[[example]]\nname = \"demo\"\npath = \"examples/demo.rs\"\n";
        let with_example = read_crate_entries(&make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", EXAMPLE_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
        ]))
        .expect("unpack");
        assert_eq!(packaged_crate_has_bin_targets(&with_example), Some(true));

        // Conventional examples/ sources count even without an explicit
        // [[example]] (belt-and-braces against implicit auto-discovery).
        let implicit_example = read_crate_entries(&make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
        ]))
        .expect("unpack");
        assert_eq!(
            packaged_crate_has_bin_targets(&implicit_example),
            Some(true)
        );
    }

    #[test]
    fn crates_equal_modulo_vcs_lockfile_drift_on_example_crate_differs() {
        // Lockfile drift on a crate carrying examples must NOT be forgiven:
        // the packaged lockfile ships to `cargo install --example` consumers.
        let local = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
        ]);
        let published = make_crate_tarball(&[
            ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
            ("c-1.0.0/src/lib.rs", b"fn a() {}"),
            ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
            ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
        ]);
        match crates_equal_modulo_vcs(&local, &published, false).expect("compare") {
            CrateContentMatch::Differs(files) => {
                assert!(
                    files.iter().any(
                        |f| f.contains("Cargo.lock") && f.contains("binary or example targets")
                    ),
                    "lockfile drift must be flagged with the install-visibility \
                     rationale: {files:?}"
                );
            }
            other => panic!("example crate lockfile drift must differ, got {other:?}"),
        }
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
        let result = is_already_published_at(
            &url,
            "foo",
            "1.2.3",
            &fast_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
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
        let result = is_already_published_at(
            &url,
            "foo",
            "1.2.3",
            &fast_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
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
        let err = is_already_published_at(
            &url,
            "foo",
            "1.2.3",
            &fast_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
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
    /// the publisher loop must emit "skipped" and NOT attempt to POST.
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
        let result = is_already_published_at(
            &url,
            "myapp",
            "1.2.3",
            &fast_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
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

    // The per-crate index confirmation is propagation progress, not a RESULT:
    // it fires once per crate with dependents, so it rides at verbose, leaving
    // `published crate '<name>'` as the only default-level per-crate output.
    #[test]
    fn index_confirmation_rides_at_verbose_not_default() {
        use anodizer_core::log::LogLevel;

        let body = r#"{"name":"myapp","vers":"1.2.3","cksum":"deadbeef","yanked":false}"#;
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![ok_resp]);
        let url = format!("http://{addr}/3/m/myapp");

        let (log, cap) =
            StageLogger::with_capture("publish-test", anodizer_core::log::Verbosity::Normal);
        poll_crates_io_index_at(&url, "myapp", "1.2.3", 5, std::time::Duration::ZERO, &log)
            .expect("version present ⇒ confirmed");

        let confirmed = "crates.io index confirmed myapp-1.2.3";
        let status: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Status)
            .map(|(_, m)| m)
            .collect();
        let verbose: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
            .map(|(_, m)| m)
            .collect();
        assert!(
            !status.iter().any(|m| m == confirmed),
            "confirmation must NOT appear at default: {status:?}"
        );
        assert!(
            verbose.iter().any(|m| m == confirmed),
            "confirmation must ride at verbose: {verbose:?}"
        );
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

    #[test]
    fn is_transient_network_failure_matches_known_signatures() {
        // The exact v0.11.3 makeself abort, verbatim from the run log.
        assert!(is_transient_network_failure(
            "    Updating crates.io index\nerror: download of ti/ny/tinystr failed\n\
             Caused by:\n  [16] Error in the HTTP2 framing layer"
        ));
        // libcurl transport faults.
        assert!(is_transient_network_failure(
            "error: failed to send HTTP request: connection refused"
        ));
        assert!(is_transient_network_failure("Connection reset by peer"));
        assert!(is_transient_network_failure(
            "error: could not resolve host: static.crates.io"
        ));
        // cargo's own wording + CDN 5xx / rate-limit.
        assert!(is_transient_network_failure(
            "warning: spurious network error (3 tries remaining)"
        ));
        assert!(is_transient_network_failure(
            "error: failed to get successful HTTP response: 503 Service Unavailable"
        ));
        assert!(is_transient_network_failure("429 Too Many Requests"));
        // Case-insensitive: curl/cargo vary casing across versions.
        assert!(is_transient_network_failure(
            "ERROR IN THE HTTP2 FRAMING LAYER"
        ));
    }

    #[test]
    fn is_transient_network_failure_rejects_unrelated_errors() {
        // Auth — a retry will not conjure a token. Must fast-fail.
        assert!(!is_transient_network_failure(
            "error: failed to publish to registry: 401 Unauthorized"
        ));
        // Packaging/validation — a broken Cargo.toml stays broken.
        assert!(!is_transient_network_failure(
            "error: invalid character `_` in crate name `bad_name`"
        ));
        // Already-published is handled upstream (idempotent skip), not by retry.
        assert!(!is_transient_network_failure(
            "error: crate version `0.11.3` is already uploaded"
        ));
        // A missing/yanked dependency surfaces "failed to download" too, but it
        // is NOT transient — retrying cannot conjure the version. The bare
        // phrase is deliberately excluded so this hard error fast-fails.
        assert!(!is_transient_network_failure(
            "error: failed to download `foo v1.2.3`\n  no matching package named `foo` found"
        ));
        // Empty stderr — don't retry.
        assert!(!is_transient_network_failure(""));
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
        // read, so the race cannot occur. When the test itself execs the
        // stub, use
        // `anodizer_core::test_helpers::fake_tool::output_retrying_etxtbsy`
        // instead of sh-routing.
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

    /// End-to-end retry on a transient network fault: the v0.11.3 regression.
    /// The stub fails twice with the exact `HTTP2 framing layer` stderr cargo
    /// emitted when makeself's publish died, then succeeds. The harness must
    /// persist through the transport blips rather than burning the re-cut.
    #[cfg(unix)]
    #[test]
    fn run_cargo_publish_with_retry_recovers_from_transient_network() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("counter");
        let stub = tmp.path().join("cargo");
        let script = format!(
            "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             if [ $n -lt 3 ]; then\n\
             echo '    Updating crates.io index' >&2\n\
             echo 'error: [16] Error in the HTTP2 framing layer' >&2\n\
             exit 101\n\
             fi\n\
             echo 'published ok'\n\
             exit 0\n",
            counter = counter.display(),
        );
        std::fs::write(&stub, script).expect("write stub");

        // Route through `sh` to dodge the ETXTBSY race exec'ing a
        // freshly-written stub under parallel tests (see the propagation test).
        let cmd = vec![
            "sh".to_string(),
            stub.display().to_string(),
            "publish".to_string(),
        ];
        let log = anodizer_core::log::StageLogger::new(
            "publish-test",
            anodizer_core::log::Verbosity::Normal,
        );
        let result = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect("retry harness must recover from a transient network blip");
        assert!(result.status.success(), "final attempt must succeed");

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
    #[serial_test::serial(stub_counter)]
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
        // STUB_COUNTER points the spawned stub at this test's own tempdir
        // counter file; the env-var NAME is shared, so the sibling
        // `..._unrelated_failure_windows` test races the set/remove pair
        // without serialization. The `#[serial(stub_counter)]` annotation on
        // the test guarantees no other stub_counter test runs concurrently.
        // SAFETY: serialised by `#[serial(stub_counter)]`; pair set / remove.
        // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
        unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
        let result = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect("retry harness must succeed after propagation lag");
        // SAFETY: serialised by `#[serial(stub_counter)]`; pair with set.
        // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
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
    #[serial_test::serial(stub_counter)]
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
        // Serialized by `#[serial(stub_counter)]` — see the sibling
        // `..._recovers_from_propagation_lag_windows` test for the
        // race this guards against.
        // SAFETY: serialised by `#[serial(stub_counter)]`; pair set / remove.
        // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
        unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
        let err = run_cargo_publish_with_retry(
            &cmd,
            "stub publish",
            &log,
            std::time::Duration::from_millis(1),
        )
        .expect_err("non-propagation failure must surface");
        // SAFETY: serialised by `#[serial(stub_counter)]`; pair with set.
        // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
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
            "starting cargo publish — processing 3 selected crate(s)"
        );
        assert_eq!(
            run_per_crate_start_message("cfgd-core"),
            "starting per-crate cargo publish for 'cfgd-core'"
        );
        assert_eq!(
            run_done_message(2),
            "finished cargo publish — 2 selected crate(s) processed"
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
        // Each crate emits `run_per_crate_start_message(name)` exactly once
        // (at verbose), in topological order, before its `(dry-run) would
        // run` line.
        cap.all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
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
        // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
        unsafe { std::env::set_var("PATH", &new_path) };
        let rb = CargoPublisher::new().rollback(&mut ctx, &evidence);
        // SAFETY: restore PATH (paired with the set above).
        unsafe {
            match prev {
                // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
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

    /// `git init` + commit everything under `dir`, yielding a CLEAN working
    /// tree the cleanliness gate (`ensure_publish_tree_clean`) can verify.
    ///
    /// The guard fails CLOSED when `git status` cannot prove cleanliness (a
    /// non-git dir errors), so these fixtures must back their `project_root`
    /// with a real repo to exercise the genuine clean-pass rather than the old
    /// fail-open hole.
    fn init_clean_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = std::process::Command::new("git");
                    cmd.current_dir(dir)
                        .args(args)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@example.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@example.com");
                    cmd
                },
                "git",
            )
            .status
            .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        // Ignore the per-test runtime scratch (cargo-stub binary + its argv
        // log) so the gate sees a CLEAN tree at entry: those files are test
        // harness artifacts, not source, and the stub's argv log is written
        // mid-run AFTER the cleanliness gate has already passed.
        std::fs::write(dir.join(".gitignore"), "cargo\nargv.log\n").expect("write .gitignore");
        run(&["add", "-A"]);
        run(&["commit", "-qm", "fixture"]);
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

    /// Fixed-tag resolver for the guard's binstall pre-publish mutation. These
    /// tests use crates with no binstall config (the emitter early-returns), so
    /// the resolver is never actually consulted; it exists only to satisfy the
    /// `publish_to_cargo_with_guard` signature without a git fixture.
    fn fixed_tag_resolver(_ctx: &Context, c: &CrateConfig) -> Option<String> {
        Some(format!("v{}", c.name))
    }

    /// Fetch closure that panics if invoked — for guard tests whose local
    /// cksum either matches the index (fast path) or must never reach the
    /// download at all (fail-closed-before-the-guard cases).
    fn fetch_panics(
        _: &str,
        _: &str,
        _: &anodizer_core::retry::RetryPolicy,
        _: &StageLogger,
    ) -> Result<Vec<u8>> {
        panic!("fetch_published must not run on this path")
    }

    /// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
    /// given `(in-tar path, content)` entries — for guard tests that must
    /// exercise the slow-path content comparison with real archive bytes.
    fn make_crate_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;

        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *content)
                .expect("append tar entry");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    /// Minimal `.cargo_vcs_info.json` body: `{"git":{"sha1":"<sha>"}}`.
    fn vcs_info_json(sha1: &str) -> Vec<u8> {
        format!(r#"{{"git":{{"sha1":"{sha1}"}}}}"#).into_bytes()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
    }

    /// Always-not-published injection: drives the publish loop straight to
    /// the `cargo publish` spawn without a sparse-index GET.
    fn never_published(
        _name: &str,
        _version: &str,
        _policy: &anodizer_core::retry::RetryPolicy,
        _log: &StageLogger,
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
        _log: &StageLogger,
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
            .project_root(tmp.path().to_path_buf())
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        init_clean_repo(tmp.path());
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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
            .project_root(tmp.path().to_path_buf())
            .build();

        // Build the evidence the failed publish would record, exactly as
        // `CargoPublisher::run` does, by driving the injected publish loop
        // and encoding whatever it recorded before the bail.
        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        init_clean_repo(tmp.path());
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .project_root(tmp.path().to_path_buf())
            .build();
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&[]);

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        init_clean_repo(tmp.path());
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
        unsafe { std::env::set_var("PATH", &new_path) };

        let publisher = CargoPublisher::new();
        let rb = publisher.rollback(&mut ctx, &evidence);

        // SAFETY: restore PATH within the same serial group.
        unsafe {
            match prev_path {
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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
        // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
        unsafe { std::env::set_var("PATH", new_path) };
        let out = f();
        // SAFETY: restore the prior PATH (paired with the set above).
        unsafe {
            match prev {
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .project_root(tmp.path().to_path_buf())
            .build();
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

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .project_root(tmp.path().to_path_buf())
            .build();
        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        evidence.extra = encode_cargo_yank_targets(&[CargoYankTarget {
            name: "crate-idx".into(),
            version: "0.2.0".into(),
            registry: None,
            index: Some("sparse+https://example.test/index/".into()),
        }]);

        // `none` never matches a publish arg, so this stub exits 0 for yank.
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        init_clean_repo(tmp.path());
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
            .project_root(tmp.path().to_path_buf())
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        // never_published would early-skip on a non-empty version, but the
        // empty-version branch bypasses the index check entirely and goes
        // straight to publish — so the stub's `cargo publish` runs.
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail-crate");
        init_clean_repo(tmp.path());
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

    /// Already-published idempotency: when the index reports the version live
    /// (`Ok(Some(cksum)`) AND the local `.crate` is byte-identical, the publish
    /// loop SKIPS that crate — `cargo publish` is never spawned and nothing is
    /// recorded. The content-vs-version guard only treats a match as a safe
    /// skip; the identical-content path is the legitimate idempotent re-cut.
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
            .project_root(tmp.path().to_path_buf())
            .build();

        // Inject "already on crates.io with this cksum" for every query, and a
        // local `.crate` checksum that MATCHES — the safe idempotent re-cut.
        let always_published =
            |_n: &str,
             _v: &str,
             _p: &anodizer_core::retry::RetryPolicy,
             _l: &StageLogger|
             -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };
        let local_matches = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "deadbeef".to_string(),
                bytes: Vec::new(),
            }))
        };

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["live-crate".to_string()],
                &log,
                &mut record,
                always_published,
                local_matches,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        result.expect("already-published-identical path returns Ok");

        assert!(
            read_argv_log(&argv_log).is_empty(),
            "already-published crate must NOT spawn cargo publish"
        );
        assert!(
            record.is_empty(),
            "a skipped (already-published) crate is not recorded for yank"
        );
    }

    /// Index-check error (`Err`) for a never-published crate (`crate_version`
    /// resolves but the index is unreachable) FAILS CLOSED: the loop refuses
    /// to skip a version it cannot confirm is byte-identical to the published
    /// artifact, because silently skipping a possibly-poisoned version is the
    /// exact failure the content-vs-version guard prevents.
    #[test]
    #[serial(cargo_stub_path)]
    fn index_check_error_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "flaky", "1.0.0");
        let argv_log = tmp.path().join("argv.log");

        let crate_cfg = cargo_crate("flaky", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["flaky".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();

        let index_errors = |_n: &str,
                            _v: &str,
                            _p: &anodizer_core::retry::RetryPolicy,
                            _l: &StageLogger|
         -> Result<Option<String>> {
            Err(anyhow::anyhow!("index transport blew up"))
        };
        let local_unused = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "unused".to_string(),
                bytes: Vec::new(),
            }))
        };

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["flaky".to_string()],
                &log,
                &mut record,
                index_errors,
                local_unused,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        let err = result.expect_err("index error must fail closed, not publish blindly");
        assert!(
            format!("{err:#}").contains("could not reach the crates.io index"),
            "fail-closed error names the network cause: {err:#}"
        );
        assert!(
            read_argv_log(&argv_log).is_empty(),
            "must NOT publish when the skip decision is unverifiable"
        );
        assert!(record.is_empty(), "nothing published ⇒ nothing recorded");
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
            .project_root(tmp.path().to_path_buf())
            .build();

        let log = StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "never");
        init_clean_repo(tmp.path());
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
            .project_root(tmp.path().to_path_buf())
            .build();

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "crate-b");
        init_clean_repo(tmp.path());
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
            .project_root(tmp.path().to_path_buf())
            .build();

        // The "1.0.0" release version IS already on crates.io — if we
        // incorrectly keyed the skip-decision on it, the crate would be
        // skipped. The correct behaviour is to attempt publish anyway because
        // the per-crate version is unresolvable.
        let always_published_1_0_0 =
            |_name: &str,
             _version: &str,
             _policy: &anodizer_core::retry::RetryPolicy,
             _l: &StageLogger|
             -> Result<Option<String>> { Ok(Some("deadbeef".to_string())) };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "none");
        init_clean_repo(tmp.path());
        let _env = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Read the previous PATH under the lock so a concurrent mutator
        // cannot interleave between the read and the set below.
        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialised by env_mutex above (shared with every other
        // PATH mutator) plus this test's serial group; paired restore below.
        // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
                Some(p) => std::env::set_var("PATH", p),
                // env-ok: PATH stub swap under #[serial(cargo_stub_path)] + env_mutex; restored on drop
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

    // ----- content-vs-version poison guard --------------------------------
    //
    // These drive `publish_to_cargo_with_guard`, injecting BOTH the crates.io
    // already-published index check AND the local `.crate` checksum computer,
    // so the guard's match/mismatch/fail-closed branches run without any
    // network round-trip or real `cargo package`.

    /// `cargo publish -p <name>` count recorded by the stub.
    fn publish_count(argv_log: &Path, name: &str) -> usize {
        read_argv_log(argv_log)
            .iter()
            .filter(|l| l.starts_with("publish") && l.contains(name))
            .count()
    }

    /// version-not-published → guard inert, crate publishes normally.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_publishes_when_version_not_on_crates_io() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "alpha", "1.0.0");
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("alpha", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["alpha".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let index_absent =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| Ok(None);
        // Local cksum must NEVER be consulted when the version is absent.
        let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            panic!("local cksum must not be computed when version is not published")
        };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["alpha".to_string()],
                &log,
                &mut record,
                index_absent,
                local_panics,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        result.expect("absent version must publish");
        assert_eq!(publish_count(&argv_log, "alpha"), 1, "alpha must publish");
    }

    /// Fail-CLOSED on an indeterminate working tree: when `project_root` is not
    /// a git repository (git status cannot prove cleanliness), the guard must
    /// REFUSE — not treat the empty/errored porcelain as "clean → proceed". A
    /// guard documented as failing loud rather than silently skipping is a
    /// poison hole if an unverifiable tree slips through. Mirrors the real risk:
    /// a manual `--publish-only` invoked from a non-repo cwd.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_refuses_when_git_status_indeterminate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "alpha", "1.0.0");
        // Deliberately NOT a git repo — `git status` errors here.
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("alpha", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["alpha".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // The version is absent so a fail-OPEN guard would proceed to publish;
        // a correct fail-CLOSED guard aborts before ever probing the index.
        let index_absent =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| Ok(None);
        let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            panic!("guard must abort on an unverifiable tree, never package")
        };

        // NB: no `init_clean_repo` here — this fixture's whole point is a
        // non-git `project_root`, so the gate must fail closed.
        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["alpha".to_string()],
                &log,
                &mut record,
                index_absent,
                local_panics,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        let err = result
            .expect_err("an indeterminate (non-git) working tree must fail the guard, not proceed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cannot verify") && msg.contains("clean git checkout"),
            "error must be actionable about the unverifiable tree: {msg}"
        );
        assert_eq!(
            publish_count(&argv_log, "alpha"),
            0,
            "nothing may publish once the guard refuses: {:?}",
            read_argv_log(&argv_log)
        );
    }

    /// already-published + local checksum IDENTICAL → safe idempotent skip.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_skips_when_already_published_identical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "beta", "2.1.0");
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("beta", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v2.1.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["beta".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let index_match = |_n: &str,
                           _v: &str,
                           _p: &anodizer_core::retry::RetryPolicy,
                           _l: &StageLogger| Ok(Some("abc123".into()));
        let local_match = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "ABC123".to_string(), // case-insensitive match
                bytes: Vec::new(),
            }))
        };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["beta".to_string()],
                &log,
                &mut record,
                index_match,
                local_match,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        result.expect("identical content must be a safe skip");
        assert_eq!(
            publish_count(&argv_log, "beta"),
            0,
            "identical already-published version must NOT re-publish"
        );
    }

    /// already-published + local content GENUINELY DIFFERENT (not just the
    /// vcs commit stamp) → the slow path fetches the published `.crate` and
    /// hard-fails on the real drift.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_hard_fails_when_already_published_different() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "gamma", "3.0.0");
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("gamma", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v3.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["gamma".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let local_bytes = make_crate_tarball(&[
            ("gamma-3.0.0/src/lib.rs", b"fn a() {}"),
            (
                "gamma-3.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a"),
            ),
        ]);
        let published_bytes = make_crate_tarball(&[
            ("gamma-3.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
            (
                "gamma-3.0.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a"),
            ),
        ]);
        let index_sha = sha256_hex(&published_bytes);
        assert_ne!(
            sha256_hex(&local_bytes),
            index_sha,
            "fixture must miss the fast path"
        );

        let index_sha_for_closure = index_sha.clone();
        let index_cksum =
            move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(Some(index_sha_for_closure.clone()))
            };
        let local_bytes_for_closure = local_bytes.clone();
        let local_differs = move |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: sha256_hex(&local_bytes_for_closure),
                bytes: local_bytes_for_closure.clone(),
            }))
        };
        let published_bytes_for_closure = published_bytes.clone();
        let fetch =
            move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(published_bytes_for_closure.clone())
            };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["gamma".to_string()],
                &log,
                &mut record,
                index_cksum,
                local_differs,
                &fixed_tag_resolver,
                fetch,
            )
        });
        let err = result.expect_err("content drift must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DIFFERENT content")
                && msg.contains("Bump the version")
                && msg.contains("gamma-3.0.0/src/lib.rs"),
            "error must explain the poison, name the differing path, and prescribe a bump: {msg}"
        );
        assert_eq!(
            publish_count(&argv_log, "gamma"),
            0,
            "poisoned version must NOT publish"
        );
    }

    /// already-published but the crates.io index is UNREACHABLE → fail closed
    /// (never silently skip a possibly-poisoned version).
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_fails_closed_when_index_unreachable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "delta", "4.2.0");
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("delta", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v4.2.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["delta".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // The dep-completeness probe at the top of the loop also consults this
        // seam; an Err there is treated as Unknown (never fails the guard), so
        // an unreachable index for a no-deps crate is benign until the skip
        // decision, where it must fail closed.
        let index_unreachable =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Err(anyhow::anyhow!("connection refused"))
            };
        let local_unused = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Ok(Some(LocalCrate {
                cksum: "unused".to_string(),
                bytes: Vec::new(),
            }))
        };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["delta".to_string()],
                &log,
                &mut record,
                index_unreachable,
                local_unused,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        let err = result.expect_err("unreachable index must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("could not reach the crates.io index")
                && msg.contains("possibly-poisoned"),
            "fail-closed error must name the network cause: {msg}"
        );
        assert_eq!(
            publish_count(&argv_log, "delta"),
            0,
            "must NOT publish when the skip decision is unverifiable"
        );
    }

    /// already-published but the local `.crate` checksum is UNCOMPUTABLE
    /// (packaging error) → fail closed; cannot prove identity, refuse to skip.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_fails_closed_when_local_cksum_uncomputable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "epsilon", "5.0.0");
        let argv_log = tmp.path().join("argv.log");
        let crate_cfg = cargo_crate("epsilon", &path, &[], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .tag("v5.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["epsilon".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let index_present =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(Some("published".into()))
            };
        let local_errs = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            Err(anyhow::anyhow!("cargo package exploded"))
        };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["epsilon".to_string()],
                &log,
                &mut record,
                index_present,
                local_errs,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        let err = result.expect_err("uncomputable local cksum must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("could not be computed") && msg.contains("cargo package exploded"),
            "fail-closed error must chain the packaging cause: {msg}"
        );
        assert_eq!(publish_count(&argv_log, "epsilon"), 0, "must not publish");
    }

    /// Custom (non-crates.io) registry → the crates.io index cksum is
    /// meaningless, so the guard is skipped and publish is attempted (the
    /// target registry's server governs idempotency). The local-cksum seam
    /// must never be consulted.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_skipped_for_custom_registry_publishes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_crate_dir(tmp.path(), "zeta", "6.0.0");
        let argv_log = tmp.path().join("argv.log");
        let cfg = CargoPublishConfig {
            registry: Some("my-corp".to_string()),
            index_timeout: Some(0),
            ..Default::default()
        };
        let crate_cfg = cargo_crate("zeta", &path, &[], cfg);
        let mut ctx = TestContextBuilder::new()
            .tag("v6.0.0")
            .crates(vec![crate_cfg])
            .selected_crates(vec!["zeta".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // Even if crates.io reports the name+version as published, a custom
        // registry must NOT trust that: attempt publish anyway.
        let index_says_published =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(Some("crates_io".into()))
            };
        let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            panic!("local cksum must not run for a non-crates.io registry")
        };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["zeta".to_string()],
                &log,
                &mut record,
                index_says_published,
                local_panics,
                &fixed_tag_resolver,
                fetch_panics,
            )
        });
        result.expect("custom registry publish must proceed");
        assert_eq!(
            publish_count(&argv_log, "zeta"),
            1,
            "custom-registry crate must publish despite a crates.io hit"
        );
    }

    /// Per-crate workspace mode: EACH published crate is checked independently
    /// against its own crates.io entry. crate-a is already published with
    /// identical content (skip); crate-b is already published with DIFFERENT
    /// content (hard fail) — so the run aborts on b. crate-a (skipped, not
    /// published this run) must NOT be recorded for rollback.
    #[test]
    #[serial(cargo_stub_path)]
    fn guard_per_crate_workspace_each_checked_independently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_a = write_crate_dir(tmp.path(), "ws-a", "0.3.0");
        let path_b = write_crate_dir(tmp.path(), "ws-b", "0.7.0");
        let argv_log = tmp.path().join("argv.log");
        // b depends on a → topological order processes a first.
        let crate_a = cargo_crate("ws-a", &path_a, &[], CargoPublishConfig::default());
        let crate_b = cargo_crate("ws-b", &path_b, &["ws-a"], CargoPublishConfig::default());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_a, crate_b])
            .selected_crates(vec!["ws-b".to_string()])
            .project_root(tmp.path().to_path_buf())
            .build();
        let log = StageLogger::new("guard-test", anodizer_core::log::Verbosity::Normal);
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // ws-a: byte-identical re-cut (fast path, no fetch). ws-b: local sha
        // misses the index (slow path), and the fetched published .crate has
        // a genuine content difference (poison → hard fail).
        let ws_b_local_bytes = make_crate_tarball(&[
            ("ws-b-0.7.0/src/lib.rs", b"fn b() {}"),
            (
                "ws-b-0.7.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a"),
            ),
        ]);
        let ws_b_published_bytes = make_crate_tarball(&[
            ("ws-b-0.7.0/src/lib.rs", b"fn b() { /* poisoned */ }"),
            (
                "ws-b-0.7.0/.cargo_vcs_info.json",
                &vcs_info_json("commit_a"),
            ),
        ]);
        let ws_b_index_sha = sha256_hex(&ws_b_published_bytes);
        assert_ne!(
            sha256_hex(&ws_b_local_bytes),
            ws_b_index_sha,
            "fixture must miss the fast path for ws-b"
        );

        // Both already published; index cksums differ per crate.
        let ws_b_index_sha_for_closure = ws_b_index_sha.clone();
        let index_per_crate = move |n: &str,
                                    _v: &str,
                                    _p: &anodizer_core::retry::RetryPolicy,
                                    _l: &StageLogger| match n {
            "ws-a" => Ok(Some("a_published".into())),
            "ws-b" => Ok(Some(ws_b_index_sha_for_closure.clone())),
            _ => Ok(None),
        };
        // a matches (safe skip, fast path); b misses the fast path and drifts
        // for real on the slow path (poison → hard fail).
        let ws_b_local_bytes_for_closure = ws_b_local_bytes.clone();
        let local_per_crate =
            move |n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| match n {
                "ws-a" => Ok(Some(LocalCrate {
                    cksum: "a_published".to_string(),
                    bytes: Vec::new(),
                })),
                "ws-b" => Ok(Some(LocalCrate {
                    cksum: sha256_hex(&ws_b_local_bytes_for_closure),
                    bytes: ws_b_local_bytes_for_closure.clone(),
                })),
                _ => Ok(None),
            };
        let ws_b_published_bytes_for_closure = ws_b_published_bytes.clone();
        let fetch =
            move |n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                assert_eq!(n, "ws-b", "only ws-b's fast path should miss");
                Ok(ws_b_published_bytes_for_closure.clone())
            };

        let new_path = install_cargo_stub(tmp.path(), &argv_log, "no-fail");
        init_clean_repo(tmp.path());
        let result = with_path(&new_path, || {
            publish_to_cargo_with_guard(
                &mut ctx,
                &["ws-b".to_string()],
                &log,
                &mut record,
                index_per_crate,
                local_per_crate,
                &fixed_tag_resolver,
                fetch,
            )
        });
        let err = result.expect_err("ws-b drift must abort the run");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ws-b") && msg.contains("DIFFERENT content"),
            "{msg}"
        );
        // Neither crate published this run; ws-a was a safe skip, ws-b poisoned.
        assert_eq!(
            publish_count(&argv_log, "ws-a"),
            0,
            "ws-a skipped, not published"
        );
        assert_eq!(
            publish_count(&argv_log, "ws-b"),
            0,
            "ws-b poisoned, not published"
        );
        assert!(
            record.is_empty(),
            "no crate published → nothing to roll back"
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

// ---------------------------------------------------------------------------
// binstall-metadata-on-publish tests
//
// The cargo publisher emits [package.metadata.binstall] into each published
// crate's on-disk Cargo.toml right before `cargo publish`, so `cargo binstall`
// fetches the prebuilt asset rather than compiling from source — even on the
// `--publish-only` path that skips the build stage entirely. These tests drive
// `ensure_binstall_metadata_with` with a fixed-tag closure (no git fixture
// needed) across single-crate and workspace per-crate modes, proving the
// emitted overrides carry each crate's OWN name_template / tag / version.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod binstall_on_publish_tests {
    use super::*;
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, BinstallConfig, Defaults, FormatOverride, GitHubConfig,
        ReleaseConfig,
    };
    use anodizer_core::log::Verbosity;
    use anodizer_core::test_helpers::TestContextBuilder;

    fn quiet_log() -> StageLogger {
        StageLogger::new("publish-test", Verbosity::Normal)
    }

    /// `git init` + commit everything under `dir`, yielding a CLEAN working
    /// tree the cleanliness gate can verify. The guard fails CLOSED when
    /// `git status` cannot prove cleanliness, so a fixture must back its
    /// `project_root` with a real repo to exercise the genuine clean-pass.
    fn init_clean_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let ok = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = std::process::Command::new("git");
                    cmd.current_dir(dir)
                        .args(args)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@example.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@example.com");
                    cmd
                },
                "git",
            )
            .status
            .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        run(&["add", "-A"]);
        run(&["commit", "-qm", "fixture"]);
    }

    /// An anodize-style archive: explicit name_template, tar.gz default, windows→zip.
    fn anodize_archive() -> ArchiveConfig {
        ArchiveConfig {
            name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: Some(vec![FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]),
            ..Default::default()
        }
    }

    /// A binstall-enabled crate rooted at `path`, owning `name`, with a GitHub
    /// release at `tj-smith47/<repo>` and the anodize-style archive.
    fn binstall_crate(name: &str, repo: &str, path: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: "v{{ Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![anodize_archive()]),
            release: Some(ReleaseConfig {
                github: Some(GitHubConfig {
                    owner: "tj-smith47".to_string(),
                    name: repo.to_string(),
                }),
                ..Default::default()
            }),
            binstall: Some(BinstallConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn write_manifest(dir: &std::path::Path, name: &str, version: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("Cargo.toml");
        std::fs::write(
            &p,
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        // A binstallable crate is a binary crate; declare the `--bin` so the
        // build-synthesis gate the override derivation routes through sees a
        // producing default build (a no-bin crate now derives no targets).
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        p
    }

    /// Read the emitted override asset leaf for `triple`, resolving the
    /// cargo-binstall `{ version }` token back to `version`.
    fn override_asset(manifest: &std::path::Path, triple: &str, version: &str) -> String {
        let doc = std::fs::read_to_string(manifest)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let url = doc["package"]["metadata"]["binstall"]["overrides"][triple]["pkg-url"]
            .as_str()
            .unwrap()
            .to_string();
        url.rsplit('/')
            .next()
            .unwrap()
            .replace("{ version }", version)
    }

    /// Single-crate mode: a lone crate gets its binstall overrides emitted with
    /// its own name_template, resolving to the real per-target asset names.
    #[test]
    fn single_crate_emits_binstall_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("app");
        let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");

        let crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .crates(vec![crate_cfg.clone()])
            .build();

        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
        ensure_binstall_metadata_with(&mut ctx, &crate_cfg, false, &quiet_log(), &fixed_tag)
            .unwrap();

        assert_eq!(
            override_asset(&manifest, "x86_64-unknown-linux-gnu", "1.2.3"),
            "anodizer-1.2.3-linux-amd64.tar.gz"
        );
        assert_eq!(
            override_asset(&manifest, "aarch64-pc-windows-msvc", "1.2.3"),
            "anodizer-1.2.3-windows-arm64.zip"
        );
    }

    /// Disabled binstall is a no-op: the manifest is left pristine.
    #[test]
    fn disabled_binstall_does_not_mutate_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("app");
        let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");
        let original = std::fs::read_to_string(&manifest).unwrap();

        let mut crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
        crate_cfg.binstall = Some(BinstallConfig {
            enabled: Some(false),
            ..Default::default()
        });
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .crates(vec![crate_cfg.clone()])
            .build();

        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
        ensure_binstall_metadata_with(&mut ctx, &crate_cfg, false, &quiet_log(), &fixed_tag)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            original,
            "disabled binstall must leave the manifest untouched"
        );
    }

    /// dry_run honored: the emitter does not mutate the manifest under dry-run.
    #[test]
    fn dry_run_does_not_mutate_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("app");
        let manifest = write_manifest(&crate_dir, "anodizer", "1.2.3");
        let original = std::fs::read_to_string(&manifest).unwrap();

        let crate_cfg = binstall_crate("anodizer", "anodizer", crate_dir.to_str().unwrap());
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .crates(vec![crate_cfg.clone()])
            .build();

        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());
        ensure_binstall_metadata_with(&mut ctx, &crate_cfg, true, &quiet_log(), &fixed_tag)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            original,
            "dry-run binstall emission must leave the manifest untouched"
        );
    }

    /// Workspace per-crate mode: two crates with DIFFERENT versions, repos, and
    /// (via the fixed-tag closure) tags. Each crate's emitted overrides must
    /// carry its OWN version/repo — never a shared/global value — proving the
    /// per-crate re-scope. This is the canonical anodize-only bug family the
    /// all-config-modes rule guards against.
    #[test]
    fn workspace_per_crate_emits_each_crates_own_version_and_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("crate-a");
        let dir_b = tmp.path().join("crate-b");
        let manifest_a = write_manifest(&dir_a, "alpha", "1.0.0");
        let manifest_b = write_manifest(&dir_b, "beta", "2.5.0");

        let crate_a = binstall_crate("alpha", "alpha-repo", dir_a.to_str().unwrap());
        let crate_b = binstall_crate("beta", "beta-repo", dir_b.to_str().unwrap());

        let mut ctx = TestContextBuilder::new()
            .project_name("alpha")
            .crates(vec![crate_a.clone(), crate_b.clone()])
            .build();

        // Each crate resolves to its OWN tag — a per-crate-cadence workspace.
        let tag_a = |_: &Context, _: &CrateConfig| Some("v1.0.0".to_string());
        let tag_b = |_: &Context, _: &CrateConfig| Some("v2.5.0".to_string());

        ensure_binstall_metadata_with(&mut ctx, &crate_a, false, &quiet_log(), &tag_a).unwrap();
        ensure_binstall_metadata_with(&mut ctx, &crate_b, false, &quiet_log(), &tag_b).unwrap();

        // crate-a: alpha @ 1.0.0 at alpha-repo.
        assert_eq!(
            override_asset(&manifest_a, "x86_64-unknown-linux-gnu", "1.0.0"),
            "alpha-1.0.0-linux-amd64.tar.gz"
        );
        let doc_a = std::fs::read_to_string(&manifest_a)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let url_a = doc_a["package"]["metadata"]["binstall"]["overrides"]
            ["x86_64-unknown-linux-gnu"]["pkg-url"]
            .as_str()
            .unwrap();
        assert!(
            url_a.contains("tj-smith47/alpha-repo") && url_a.contains("/v{ version }/"),
            "crate-a override must target its OWN repo + tag token, got: {url_a}"
        );

        // crate-b: beta @ 2.5.0 at beta-repo — NOT alpha's version/repo.
        assert_eq!(
            override_asset(&manifest_b, "aarch64-apple-darwin", "2.5.0"),
            "beta-2.5.0-darwin-arm64.tar.gz"
        );
        let doc_b = std::fs::read_to_string(&manifest_b)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let url_b = doc_b["package"]["metadata"]["binstall"]["overrides"]["aarch64-apple-darwin"]
            ["pkg-url"]
            .as_str()
            .unwrap();
        assert!(
            url_b.contains("tj-smith47/beta-repo"),
            "crate-b override must target its OWN repo, not crate-a's, got: {url_b}"
        );
        assert!(
            !url_b.contains("alpha"),
            "crate-b override must not leak crate-a's name/version, got: {url_b}"
        );
    }

    /// `defaults.targets` drives the override set when no per-build targets are
    /// configured — `resolve_default_targets` must mirror the build stage so the
    /// emitted triples equal the released asset set.
    #[test]
    fn resolve_default_targets_honors_config_then_falls_back() {
        // Explicit defaults.targets wins.
        let ctx = TestContextBuilder::new()
            .defaults(Defaults {
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                ..Default::default()
            })
            .build();
        assert_eq!(
            resolve_default_targets(&ctx),
            vec!["x86_64-unknown-linux-gnu".to_string()]
        );

        // No defaults.targets → canonical DEFAULT_TARGETS (the six-triple matrix).
        let ctx2 = TestContextBuilder::new().build();
        assert_eq!(
            resolve_default_targets(&ctx2),
            anodizer_core::target::DEFAULT_TARGETS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // Guard-ordering tests: the poison guard must package the SAME tree state
    // `cargo publish` uploads, including anodizer's own pre-publish binstall
    // mutation. These drive the full `publish_to_cargo_with_guard` loop with an
    // injected local-cksum that READS the on-disk Cargo.toml, so the recorded
    // hash reflects whether the binstall table was written before the guard ran.
    // -----------------------------------------------------------------------

    /// True when the crate at `path` carries `[package.metadata.binstall]` in
    /// its on-disk Cargo.toml. The stand-in for "the .crate bytes differ with
    /// vs without the binstall table" — without re-implementing `cargo package`.
    fn has_binstall_table(path: &str) -> bool {
        let manifest = std::path::Path::new(path).join("Cargo.toml");
        std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
            .map(|doc| {
                doc.get("package")
                    .and_then(|p| p.get("metadata"))
                    .and_then(|m| m.get("binstall"))
                    .is_some()
            })
            .unwrap_or(false)
    }

    /// A cargo cfg with the post-publish index poll disabled (no dependents in
    /// these single-crate fixtures) so the loop never waits on the real index.
    fn no_poll_cargo_cfg() -> CargoPublishConfig {
        CargoPublishConfig {
            index_timeout: Some(0),
            ..Default::default()
        }
    }

    fn binstall_crate_for_publish(name: &str, repo: &str, path: &str) -> CrateConfig {
        let mut c = binstall_crate(name, repo, path);
        c.publish = Some(anodizer_core::config::PublishConfig {
            cargo: Some(no_poll_cargo_cfg()),
            ..Default::default()
        });
        c
    }

    /// Fetch closure that panics if invoked — for guard tests whose local
    /// cksum matches the index (fast path) or never reaches the download.
    fn fetch_panics(
        _: &str,
        _: &str,
        _: &anodizer_core::retry::RetryPolicy,
        _: &StageLogger,
    ) -> Result<Vec<u8>> {
        panic!("fetch_published must not run on this path")
    }

    /// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
    /// given `(in-tar path, content)` entries — for the negative-control test
    /// that must exercise the slow-path content comparison with real bytes.
    fn make_crate_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;

        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *content)
                .expect("append tar entry");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
    }

    /// BLOCKER reproduction: a binstall crate, already published with the
    /// WITH-binstall content (as the original publish uploaded), must be a SAFE
    /// SKIP on re-cut — NOT a false poison. The guard now writes the binstall
    /// table before packaging, so the local hash reflects the same tree the
    /// original `cargo publish` shipped. (Before the fix, the guard packaged the
    /// pre-binstall tree → local "WITHOUT" ≠ index "WITH" → false hard-fail.)
    #[test]
    fn guard_skips_binstall_crate_when_recut_matches_published() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("cli");
        write_manifest(&crate_dir, "anodizer", "1.2.3");
        let path = crate_dir.to_str().unwrap();
        let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);

        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .tag("v1.2.3")
            .crates(vec![crate_cfg.clone()])
            .selected_crates(vec!["anodizer".to_string()])
            .build();
        // Commit the fixture tree so the cleanliness gate verifies a genuine
        // CLEAN repo (not the old fail-open hole), isolating the skip path.
        init_clean_repo(tmp.path());
        ctx.options.project_root = Some(tmp.path().to_path_buf());
        let log = quiet_log();
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // The version is on crates.io; its recorded cksum is the WITH-binstall
        // marker (what the original publish, which wrote the table, uploaded).
        let index_with_binstall =
            |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(Some("WITH".into()))
            };
        // The local-cksum stub hashes the REAL on-disk tree: "WITH" iff the
        // binstall table is present at the moment the guard packages.
        let local_reads_disk = |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            let marker = if has_binstall_table(&c.path) {
                "WITH"
            } else {
                "WITHOUT"
            };
            Ok(Some(LocalCrate {
                cksum: marker.to_string(),
                bytes: Vec::new(),
            }))
        };
        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());

        publish_to_cargo_with_guard(
            &mut ctx,
            &["anodizer".to_string()],
            &log,
            &mut record,
            index_with_binstall,
            local_reads_disk,
            &fixed_tag,
            fetch_panics,
        )
        .expect(
            "a binstall crate re-cut whose published content already includes the binstall table \
             must be a SAFE SKIP, not a false poison",
        );
        assert!(
            record.is_empty(),
            "a safe skip publishes nothing — nothing to record"
        );
    }

    /// Negative control proving the fix is load-bearing: if the index recorded
    /// the WITHOUT-binstall content (a crate published BEFORE anodizer started
    /// writing the table), the guard — which now packages WITH the table — would
    /// see a genuine content divergence and hard-fail. This demonstrates the
    /// guard still flags real drift; it isn't blanket-skipping binstall crates.
    #[test]
    fn guard_flags_real_drift_even_for_binstall_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("cli");
        write_manifest(&crate_dir, "anodizer", "9.9.9");
        let path = crate_dir.to_str().unwrap();
        let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);

        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .tag("v9.9.9")
            .crates(vec![crate_cfg.clone()])
            .selected_crates(vec!["anodizer".to_string()])
            .build();
        // Committed clean repo (see the sibling skip test) → the gate passes
        // on merit and this test exercises the genuine drift hard-fail.
        init_clean_repo(tmp.path());
        ctx.options.project_root = Some(tmp.path().to_path_buf());
        let log = quiet_log();
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // Real tarball bytes standing in for "packaged WITH the binstall
        // table" vs "published WITHOUT it" — a genuine content divergence,
        // not just the vcs commit stamp, so the slow path must hard-fail.
        let with_binstall_bytes = make_crate_tarball(&[(
            "anodizer-9.9.9/Cargo.toml",
            b"[package]\nname = \"anodizer\"\n\n[package.metadata.binstall]\npkg-url = \"x\"\n",
        )]);
        let without_binstall_bytes = make_crate_tarball(&[(
            "anodizer-9.9.9/Cargo.toml",
            b"[package]\nname = \"anodizer\"\n",
        )]);
        let index_sha = sha256_hex(&without_binstall_bytes);

        let index_sha_for_closure = index_sha.clone();
        let index_without_binstall =
            move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(Some(index_sha_for_closure.clone()))
            };
        let with_binstall_bytes_for_local = with_binstall_bytes.clone();
        let without_binstall_bytes_for_local = without_binstall_bytes.clone();
        let local_reads_disk =
            move |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
                // The guard's pre-publish mutation writes the binstall table
                // before packaging; a real `cargo package` here would reflect it,
                // so the stub packages the "WITH" fixture whenever the on-disk
                // manifest carries the table (as it does after that mutation).
                let bytes = if has_binstall_table(&c.path) {
                    with_binstall_bytes_for_local.clone()
                } else {
                    without_binstall_bytes_for_local.clone()
                };
                Ok(Some(LocalCrate {
                    cksum: sha256_hex(&bytes),
                    bytes,
                }))
            };
        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v9.9.9".to_string());
        let fetch =
            move |_n: &str, _v: &str, _p: &anodizer_core::retry::RetryPolicy, _l: &StageLogger| {
                Ok(without_binstall_bytes.clone())
            };

        let err = publish_to_cargo_with_guard(
            &mut ctx,
            &["anodizer".to_string()],
            &log,
            &mut record,
            index_without_binstall,
            local_reads_disk,
            &fixed_tag,
            fetch,
        )
        .expect_err("a genuine content divergence must still hard-fail");
        assert!(
            format!("{err:#}").contains("DIFFERENT content"),
            "must report the poison, not silently skip: {err:#}"
        );
    }

    /// Multi-crate regression: crate A's pre-publish binstall write dirties the
    /// tree, but it must NOT false-trip the cleanliness check for crate B. The
    /// check runs ONCE before the loop, on a tree clean at entry, so both
    /// binstall crates re-cut safely. (A per-crate check would have seen A's
    /// write and wrongly aborted B.)
    #[test]
    fn guard_clean_check_runs_once_not_per_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        write_manifest(&dir_a, "alpha", "1.0.0");
        write_manifest(&dir_b, "beta", "1.0.0");
        let mut crate_a = binstall_crate_for_publish("alpha", "alpha", dir_a.to_str().unwrap());
        // b depends on a → topological order processes a first, so a's binstall
        // write lands before b's iteration.
        crate_a.depends_on = Some(vec![]);
        let mut crate_b = binstall_crate_for_publish("beta", "beta", dir_b.to_str().unwrap());
        crate_b.depends_on = Some(vec!["alpha".to_string()]);

        let mut ctx = TestContextBuilder::new()
            .project_name("alpha")
            .tag("v1.0.0")
            .crates(vec![crate_a, crate_b])
            .selected_crates(vec!["beta".to_string()])
            .build();
        // Committed clean repo → the once-before-loop gate passes on merit;
        // crate A's later in-loop binstall write must NOT retroactively trip it.
        init_clean_repo(tmp.path());
        ctx.options.project_root = Some(tmp.path().to_path_buf());
        let log = quiet_log();
        let mut record: Vec<CargoYankTarget> = Vec::new();

        // Both already published with WITH-binstall content → both safe skip.
        let index_with = |_n: &str,
                          _v: &str,
                          _p: &anodizer_core::retry::RetryPolicy,
                          _l: &StageLogger| Ok(Some("WITH".into()));
        let local_reads_disk = |_n: &str, c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            let m = if has_binstall_table(&c.path) {
                "WITH"
            } else {
                "WITHOUT"
            };
            Ok(Some(LocalCrate {
                cksum: m.to_string(),
                bytes: Vec::new(),
            }))
        };
        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.0.0".to_string());

        publish_to_cargo_with_guard(
            &mut ctx,
            &["beta".to_string()],
            &log,
            &mut record,
            index_with,
            local_reads_disk,
            &fixed_tag,
            fetch_panics,
        )
        .expect("crate A's binstall write must not false-trip the dirty check for crate B");
        assert!(
            record.is_empty(),
            "both crates safe-skipped → nothing recorded"
        );
    }

    /// WARN coverage: a DIRTY working tree at guard entry is an unverifiable
    /// precondition — the guard must STOP with an actionable error (not skip,
    /// not hard-fail on content). Uses a real git fixture so
    /// `git status --porcelain` reports the uncommitted change.
    #[test]
    fn guard_refuses_dirty_tree_before_binstall_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // A minimal git repo with one committed crate, then an uncommitted edit.
        let run_git = |args: &[&str]| {
            let ok = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = std::process::Command::new("git");
                    cmd.current_dir(repo).args(args);
                    cmd
                },
                "git",
            )
            .status
            .success();
            assert!(ok, "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "t@example.com"]);
        run_git(&["config", "user.name", "t"]);
        let crate_dir = repo.join("cli");
        write_manifest(&crate_dir, "anodizer", "1.2.3");
        run_git(&["add", "-A"]);
        run_git(&["commit", "-qm", "init"]);
        // Dirty the tree: an uncommitted source edit.
        std::fs::write(crate_dir.join("extra.rs"), "// uncommitted\n").unwrap();

        let path = crate_dir.to_str().unwrap();
        let crate_cfg = binstall_crate_for_publish("anodizer", "anodizer", path);
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .tag("v1.2.3")
            .crates(vec![crate_cfg.clone()])
            .selected_crates(vec!["anodizer".to_string()])
            .build();
        // Point the cleanliness check at the fixture repo, not the process cwd.
        ctx.options.project_root = Some(repo.to_path_buf());
        let log = quiet_log();
        let mut record: Vec<CargoYankTarget> = Vec::new();

        let index_present = |_n: &str,
                             _v: &str,
                             _p: &anodizer_core::retry::RetryPolicy,
                             _l: &StageLogger| Ok(Some("WITH".into()));
        // Must never be reached — the dirty check aborts before packaging.
        let local_panics = |_n: &str, _c: &CrateConfig, _cfg: Option<&CargoPublishConfig>| {
            panic!("local cksum must not run against a dirty tree")
        };
        let fixed_tag = |_: &Context, _: &CrateConfig| Some("v1.2.3".to_string());

        let err = publish_to_cargo_with_guard(
            &mut ctx,
            &["anodizer".to_string()],
            &log,
            &mut record,
            index_present,
            local_panics,
            &fixed_tag,
            fetch_panics,
        )
        .expect_err("a dirty tree is an unverifiable precondition; the guard must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DIRTY") && msg.contains("clean checkout") && msg.contains("extra.rs"),
            "error must be actionable (name the dirtiness + the remedy): {msg}"
        );
    }
}
