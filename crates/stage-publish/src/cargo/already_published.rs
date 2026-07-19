//! The already-published skip decision: comparing a locally packaged crate
//! against the version live on crates.io to decide skip-vs-publish on a re-cut.

use super::*;

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
pub(crate) fn is_already_published(
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
pub(crate) fn is_already_published_at(
    url: &str,
    crate_name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Option<String>> {
    match fetch_index_file(url, crate_name, policy, log)? {
        Some(body) => Ok(parse_index_cksum_for_version(&body, version)),
        None => Ok(None),
    }
}

/// GET a crate's sparse-index file: `Ok(Some(body))` on 200, `Ok(None)` on a
/// definitive 404 (the crate has never been published under this name), `Err`
/// on transport failure / retry exhaustion. The single fetch shared by the
/// name@version check ([`is_already_published_at`]) and the crate-level
/// existence probe ([`crate_exists_on_index`]).
fn fetch_index_file(
    url: &str,
    crate_name: &str,
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

    match result {
        Ok((_status, body)) => Ok(Some(body)),
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
            Err(err)
        }
    }
}

/// Whether a crate NAME exists on crates.io at all (any version) — the
/// crate-level probe behind the Trusted-Publishing new-crate guard. Distinct
/// from [`is_already_published`], which answers for one name@version and
/// cannot tell "brand-new crate" apart from "existing crate, new version"
/// (both are `Ok(None)` there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrateIndexExistence {
    /// The sparse index serves a file for this crate — some version has
    /// published before.
    Exists,
    /// The index definitively 404s the crate file — no version has ever
    /// published under this name.
    NeverPublished,
    /// Transport failure / retry exhaustion: existence could not be
    /// determined. Consumers fail OPEN — an unreachable index must never
    /// block a release whose crates are actually fine.
    Unknown,
}

/// Probe the crates.io sparse index for `crate_name`'s existence at any
/// version. Never returns an error: indeterminate outcomes collapse to
/// [`CrateIndexExistence::Unknown`] so callers are fail-open by construction.
pub(crate) fn crate_exists_on_index(
    crate_name: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> CrateIndexExistence {
    match fetch_index_file(&sparse_index_url(crate_name), crate_name, policy, log) {
        Ok(Some(_)) => CrateIndexExistence::Exists,
        Ok(None) => CrateIndexExistence::NeverPublished,
        Err(_) => CrateIndexExistence::Unknown,
    }
}

/// Local `.crate` package produced by [`local_crate_cksum`]: the sha256 the
/// fast path compares against the crates.io index `cksum`, plus the raw
/// tarball bytes the slow path (`crates_equal_modulo_vcs`) needs when the
/// fast path doesn't match.
pub(crate) struct LocalCrate {
    pub(crate) cksum: String,
    pub(crate) bytes: Vec<u8>,
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
pub(crate) fn local_crate_cksum(
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
pub(crate) fn fetch_published_crate(
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
pub(crate) enum RecutNormalization {
    /// Crate-root `.cargo_vcs_info.json`, compared modulo its `git.sha1`
    /// field (the release commit stamp of a same-source re-cut).
    VcsCommitStamp,
    /// Crate-root `CHANGELOG.md`, forgiven because the bump commit that last
    /// touched it carries anodizer's regeneration provenance marker for this
    /// crate@version.
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
            Self::ChangelogRegenerated => "CHANGELOG.md (bump-commit regeneration provenance)",
            Self::LockfileLibOnly => "Cargo.lock (lib-only crate)",
        }
    }
}

/// Outcome of comparing a local `.crate` tarball against the published one,
/// modulo anodizer's own release-process artifacts (see
/// [`RecutNormalization`]).
#[derive(Debug)]
pub(crate) enum CrateContentMatch {
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
pub(crate) fn read_crate_entries(
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
pub(crate) fn packaged_crate_has_bin_targets(
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
/// - `CHANGELOG.md` — forgiven ONLY when `changelog_regenerated` is true
///   (the bump-commit history carries a provenance marker proving anodizer
///   regenerated the file for this crate@version — see
///   [`changelog_provenance_recorded`]), so the drift is the tool's own
///   artifact. Without that provenance the drift is operator-authored (or
///   belongs to a different version) and stays a hard divergence.
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
pub(crate) fn crates_equal_modulo_vcs(
    local_crate: &[u8],
    published_crate: &[u8],
    changelog_regenerated: bool,
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
            if changelog_regenerated {
                normalized.push(RecutNormalization::ChangelogRegenerated);
            } else {
                differs.push(format!(
                    "{path_str} (not treated as a release-process artifact: the last commit \
                     touching this crate's CHANGELOG.md carries no `changelog regenerated for \
                     <crate>@<version>` provenance marker, so there is no proof anodizer \
                     regenerated this file and the drift is treated as real. If anodizer DID \
                     regenerate it, re-cut the version via `anodizer tag --changelog` so the \
                     bump commit records the marker; if the bump commit exists but is outside \
                     a shallow clone's history, use a full-depth checkout — actions/checkout \
                     `fetch-depth: 0`)"
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
pub(crate) enum CargoSkipDecision {
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
///    - `CHANGELOG.md` (only when `changelog_regenerated` — a bump-commit
///      provenance marker proves anodizer regenerated it for this
///      crate@version; see [`changelog_provenance_recorded`]);
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
pub(crate) fn decide_already_published(
    name: &str,
    version: &str,
    index_cksum: &str,
    crate_cfg: &CrateConfig,
    cargo_cfg: Option<&CargoPublishConfig>,
    changelog_regenerated: bool,
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

    match crates_equal_modulo_vcs(&local.bytes, &published_bytes, changelog_regenerated)? {
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
