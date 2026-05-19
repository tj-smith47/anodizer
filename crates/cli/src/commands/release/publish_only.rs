//! `anodize release --publish-only`: consume a `dist/` populated by
//! `anodize check determinism --preserve-dist=<path>` and run only the
//! sign + publish pipeline.
//!
//! The harness writes:
//! - `<preserved-dist>/**` — the byte-stable artifacts the determinism
//!   check just verified (archives, packages, sboms, checksums,
//!   `artifacts.json`, `metadata.json`).
//! - `<preserved-dist>/context.json` — a [`PreservedDistContext`]
//!   manifest pinning `(artifacts, targets, version, commit)`.
//!
//! This mode loads both, rehydrates `ctx.artifacts` from
//! `dist/artifacts.json` (the in-process registry shape — the manifest
//! the post-pipeline already writes), strips any leftover
//! `Signature` / `Certificate` artifacts the harness may have produced
//! with ephemeral keys, then runs an extended publish pipeline that
//! prepends `SignStage` (production-keys sign pass) ahead of the usual
//! release / publish / blob / snapcraft-publish chain.
//!
//! Idempotence: the harness skips its in-loop `SignStage` when
//! production keys are exported on the runner (`COSIGN_KEY` /
//! `GPG_PRIVATE_KEY`), so preserved-dist usually has no `.sig` /
//! `.asc` files. This module's defensive strip exists for the case
//! where that gate didn't fire (harness ran without prod keys then
//! operator brought them in later, etc.) — re-signing on top of an
//! existing signature chain would produce `*.sig.sig` chaos.
//!
//! Pipeline choice: the merge pipeline assumes raw-binary input from
//! `--split`. `--publish-only` deliberately bypasses that assumption:
//! input is the FULL artifact set (binaries + archives + packages +
//! checksums), so we run `build_publish_only_pipeline` (see
//! `crate::pipeline`), not `build_merge_pipeline`.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::git::short_commit_str;
use anodizer_core::log::StageLogger;

use super::helpers;
use crate::pipeline;

/// Names of the env vars that gate the publish-only credential
/// preflight. Documented as a single source of truth so the error
/// message and the check itself stay in lockstep.
const SIGN_ENV_VARS: &[&str] = &["COSIGN_KEY", "GPG_PRIVATE_KEY"];
const GITHUB_TOKEN_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "ANODIZER_GITHUB_TOKEN"];

/// Knobs the dispatcher hands to `publish_only::run`. Reduces the
/// number of positional arguments and lets the dispatch site speak
/// in terms of flag intent (`no_preflight`) rather than the threaded
/// `--<flag>` boolean it came from.
pub(super) struct RunOpts {
    pub dry_run: bool,
    /// `--no-preflight`: skip the credential preflight as well as the
    /// publisher-state preflight. Operator opt-out for the case
    /// where they know what they're doing and want the mid-pipeline
    /// failure to surface instead.
    pub no_preflight: bool,
}

/// `--publish-only` entry point. Wired from `commands/release/mod.rs::run`
/// after `setup_context` / git context / preflight have already run on
/// `ctx`.
pub(super) fn run(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    opts: RunOpts,
) -> Result<()> {
    log.status("running in publish-only mode (load preserved dist + sign + publish)...");

    let dist = config.dist.clone();

    // ── Pre-flight credential check ────────────────────────────────────
    // Bail BEFORE any state mutation. Spec section C: "Pre-flight
    // credential check at top of publish-only". Dry-run skips so
    // operators can preview the pipeline without secrets;
    // `--no-preflight` is the explicit operator opt-out for the rare
    // case where they want the mid-pipeline failure instead.
    if opts.dry_run {
        log.verbose("(dry-run) skipping production-credential preflight");
    } else if opts.no_preflight {
        log.warn(
            "credential preflight skipped via --no-preflight; \
             missing credentials will fail mid-pipeline (no idempotent recovery)",
        );
    } else {
        preflight_credentials(|k| std::env::var(k).ok())?;
    }

    // ── Load preserved-dist context ────────────────────────────────────
    // Two manifest families live in `<dist>/`:
    //   - `artifacts.json` / `artifacts-<shard>.json`: the canonical
    //     in-process registry shape (`kind` / `target` / `metadata`),
    //     same as `anodize publish` consumes. Each shard emits its own.
    //   - `context.json` / `context-<shard>.json`: the harness's
    //     `PreservedDistContext` summary with per-artifact `sha256` +
    //     `size` recorded at preserve time. Each shard emits its own.
    //
    // The sharded release workflow uploads each shard's dist tree under
    // `dist-<shard>` and the action renames the per-shard manifests so
    // download-artifact's `merge-multiple: true` doesn't collide on
    // identically-named files. Discovery here folds them all back in.
    //
    // The legacy single-`context.json` layout (operator running locally
    // without sharding) keeps working — `discover_preserved_contexts`
    // matches both the un-suffixed and the suffixed forms.
    let preserved_contexts = discover_preserved_contexts(&dist)?;
    let preserved = merge_preserved_contexts(&preserved_contexts);
    let shard_count = preserved_contexts.len();

    log.status(&format!(
        "publish-only: loaded {} context manifest(s) (version={}, commit={}, targets=[{}], {} artifact(s))",
        shard_count,
        preserved.version,
        short_commit_str(&preserved.commit),
        preserved.targets.join(", "),
        preserved.artifacts.len(),
    ));

    // Cross-check `commit`: fail CLOSED, not open. If either side is
    // empty we bail because we cannot prove the preserved bytes match
    // the current release. Re-signing only swaps signature blobs;
    // ever shipping a signature over bytes from a different commit
    // breaks the Safety Property invariant (artifacts shipped to
    // GitHub Release MUST have passed the determinism check).
    if preserved.commit.is_empty() {
        anyhow::bail!(
            "publish-only: no context manifest carried a `commit` field. Cannot verify the \
             preserved bytes match the current release; re-run \
             `anodize check determinism --preserve-dist=...` with a producer that \
             records the commit SHA."
        );
    }
    // Sharded layouts: every shard's `commit` MUST agree with the
    // merged value. A mismatch means two shards were preserved from
    // two different release attempts — re-signing across that mix
    // would publish bytes whose determinism guarantee is split across
    // commits.
    for (path, ctx_entry) in &preserved_contexts {
        if !ctx_entry.commit.is_empty() && ctx_entry.commit != preserved.commit {
            anyhow::bail!(
                "publish-only: shard manifest {} records commit {} but the merged set is \
                 anchored at {}. A multi-shard preserved dist must come from a single \
                 release attempt; mixing bytes from different commits would publish \
                 signatures whose determinism-verified state is split.",
                path.display(),
                short_commit_str(&ctx_entry.commit),
                short_commit_str(&preserved.commit),
            );
        }
    }
    let ctx_commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();
    if ctx_commit.is_empty() {
        anyhow::bail!(
            "publish-only: current release context has no resolved commit. \
             Run from a tagged commit (`git checkout {}`) before --publish-only.",
            short_commit_str(&preserved.commit),
        );
    }
    if ctx_commit != preserved.commit {
        anyhow::bail!(
            "publish-only: context manifest was preserved at commit {} but the current \
             release context resolved to commit {}. Re-signing the preserved bytes \
             under the current commit's tag would ship signatures that don't match \
             the determinism-verified state. `git checkout {}` then retry.",
            short_commit_str(&preserved.commit),
            short_commit_str(&ctx_commit),
            short_commit_str(&preserved.commit),
        );
    }

    // ── Rehydrate ctx.artifacts ────────────────────────────────────────
    // Delegates to the same loader `anodize publish` uses so the two
    // entry points stay in lockstep (one parser to maintain). Each
    // discovered manifest contributes its artifacts to the registry;
    // duplicate paths land in the registry as duplicates, which the
    // downstream stages tolerate (sharded matrices never overlap their
    // target sets, so duplicates would indicate a workflow bug worth
    // surfacing rather than silently de-duping).
    let artifact_manifests = discover_artifacts_manifests(&dist)?;
    for manifest_path in &artifact_manifests {
        helpers::load_artifacts_from_manifest(ctx, &dist, manifest_path).with_context(|| {
            format!(
                "publish-only: failed to load {} from {}. The preserve-dist \
                 flow normally copies these from the harness's worktree post-pipeline; \
                 if any is missing the preserved dist is incomplete.",
                manifest_path.display(),
                dist.display()
            )
        })?;
    }

    log.status(&format!(
        "publish-only: rehydrated {} artifact(s) from {} artifacts manifest(s)",
        ctx.artifacts.all().len(),
        artifact_manifests.len(),
    ));

    // ── Strip ephemeral signatures / certificates ──────────────────────
    // Defensive: the harness Phase-1 work skips SignStage when
    // production keys are set on the runner, so preserved-dist
    // usually has no `.sig` / `.asc` files. But re-signing on top
    // of an existing chain (e.g. operator ran the harness without
    // prod keys, then brought them in) would emit `*.sig.sig` /
    // `*.pem.sig` — corrupt checksums and confuse downstream
    // verifiers. Strip up-front so `SignStage` always sees a clean
    // input registry.
    strip_ephemeral_signatures(ctx, log);

    // ── Run the extended publish pipeline ──────────────────────────────
    // `build_publish_only_pipeline` is `[SignStage, ReleaseStage,
    // PublishStage, BlobStage, SnapcraftPublishStage]` — the head
    // SignStage is the production-keys re-sign pass that spec
    // section D.1 requires. Distinct from the legacy
    // `build_publish_pipeline` (consumed by `anodize publish`) which
    // does NOT prepend SignStage; conflating them would silently
    // introduce a new credential requirement to `anodize publish`.
    let p = pipeline::build_publish_only_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        super::run_post_pipeline(ctx, config, opts.dry_run, log)?;
    }

    // Same gate as `release` / `--merge`: required-publisher failures
    // must surface as a non-zero exit even though per-publisher
    // failures are non-fatal inside the pipeline body.
    if result.is_ok() {
        super::gate_required_failures(ctx)?;
    }

    result
}

/// Pre-flight credential check. Fires BEFORE any state mutation so a
/// credential miss doesn't leave a partially-uploaded release behind.
///
/// Required: a GitHub-shaped token (release stage needs to upload
/// assets / create the release) AND at least one of the production
/// signing keys (sign stage re-signs the preserved archives). Other
/// publisher credentials (chocolatey api key, AUR ssh key, etc.) are
/// per-publisher and surface at dispatch time — the pre-flight
/// publisher-state check (separate code path, runs before this branch
/// in `commands/release/mod.rs`) already validates them.
///
/// Pragmatic intentionally: the user is expected to drive
/// publish-only from CI where the secrets are exported into env
/// once. Re-deriving "which env vars matter per publisher" lives in
/// each stage's own preflight — duplicating it here would diverge.
///
/// **Env injection** (`env`): callers pass a closure that resolves
/// env-var names to values. The production caller delegates to
/// `std::env::var`; unit tests pass a pure closure so test execution
/// doesn't race with sibling tests on shared process env. This
/// mirrors how `stage-sign::helpers::should_sign_artifact` is
/// independently testable.
fn preflight_credentials(env: impl Fn(&str) -> Option<String>) -> Result<()> {
    let token_present = GITHUB_TOKEN_ENV_VARS
        .iter()
        .any(|v| env(v).map(|s| !s.is_empty()).unwrap_or(false));
    let sign_key_present = SIGN_ENV_VARS
        .iter()
        .any(|v| env(v).map(|s| !s.is_empty()).unwrap_or(false));

    if !token_present {
        anyhow::bail!(
            "publish-only: missing release token. Set one of {} before running --publish-only \
             (or pass --dry-run to preview without secrets).",
            GITHUB_TOKEN_ENV_VARS.join(" / "),
        );
    }
    if !sign_key_present {
        anyhow::bail!(
            "publish-only: missing production signing key. Set at least one of {} before \
             running --publish-only (or pass --dry-run to preview without secrets). \
             The harness's ephemeral signatures are NOT shippable — this mode exists \
             to overlay production signatures on the byte-stable artifacts.",
            SIGN_ENV_VARS.join(" / "),
        );
    }
    Ok(())
}

/// Strip `Signature` / `Certificate` artifacts the harness may have
/// left behind (with ephemeral keys). `SignStage`'s own
/// `should_sign_artifact` already filters Signature/Certificate kinds
/// out of the `all`/`any` artifacts set so a no-op re-sign wouldn't
/// emit `.sig.sig`, but the resulting registry would still include
/// the ephemeral artifacts — which then get UPLOADED by `ReleaseStage`.
/// We must remove them at the source.
///
/// Symmetry note: any non-signature/certificate artifact remains
/// untouched, including any `Checksum` entries — re-signing produces
/// a new signature blob but the underlying archive bytes (which the
/// checksums are computed from) are unchanged, so the checksums
/// still match. `ChecksumStage` is intentionally not in the publish
/// pipeline for the same reason: nothing to recompute.
fn strip_ephemeral_signatures(ctx: &mut Context, log: &StageLogger) {
    use anodizer_core::artifact::ArtifactKind;
    let stale_paths: Vec<std::path::PathBuf> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| matches!(a.kind, ArtifactKind::Signature | ArtifactKind::Certificate))
        .map(|a| a.path.clone())
        .collect();
    if stale_paths.is_empty() {
        return;
    }
    let count = stale_paths.len();
    log.status(&format!(
        "publish-only: stripping {count} ephemeral signature/certificate artifact(s) before re-sign"
    ));
    // Also delete the on-disk files so the next sign-stage doesn't see
    // a leftover `.sig` next to its target and produce a `*.sig.sig`
    // through the user's own sign-args template (which typically reads
    // `{{ .Signature }} = {{ .Artifact }}.sig`).
    let mut disk_removed = 0usize;
    for p in &stale_paths {
        match std::fs::remove_file(p) {
            Ok(()) => disk_removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log.warn(&format!(
                "publish-only: failed to delete stale signature {}: {} \
                 (continuing; SignStage will overwrite or fail loudly)",
                p.display(),
                e
            )),
        }
    }
    ctx.artifacts.remove_by_paths(&stale_paths);
    // Positive success signal so the operator sees the strip happened
    // (counter-balances the lone "stripping N..." line above which
    // could otherwise look like the work stalled). Reports both the
    // registry-side removal count (always equal to `count`) and the
    // disk-side count (may be lower if a sig was already absent on
    // disk — registry entries can outlive their files when the
    // post-pipeline runs partial writes).
    log.status(&format!(
        "publish-only: stripped {count} ephemeral signature artifact(s) from registry \
         ({disk_removed} also deleted from disk)"
    ));
}

/// Minimal `PreservedDistContext` deserializer. We re-declare the
/// shape here rather than depending on `determinism_harness::preserve`
/// to keep this module decoupled from harness internals — the
/// schema (artifacts + targets + version + commit) is the
/// load-bearing contract, not the producer module.
///
/// `#[serde(default)]` on every field so a partially-written
/// `context.json` from a buggy producer doesn't kill the load — the
/// downstream artifact-load step is the real gate. Missing fields
/// degrade gracefully (empty targets / version / commit).
#[derive(serde::Deserialize, Debug, Default, Clone)]
struct PreservedDistContext {
    #[serde(default)]
    artifacts: Vec<PreservedArtifact>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    commit: String,
}

/// Per-artifact entry in `context.json`. The harness's writer always
/// populates `sha256` + `size`, but the publish-only path doesn't yet
/// consume them — they're reserved for a forthcoming "hash-verify"
/// mode that cross-checks each preserved file's bytes against the
/// determinism report before re-signing.
#[allow(dead_code)] // `name`/`path`/`sha256`/`size` retained for hash-verify mode
#[derive(serde::Deserialize, Debug, Default, Clone)]
struct PreservedArtifact {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    size: u64,
}

/// Walk `dist/` for every `context.json` and `context-*.json` entry at
/// the dist root (non-recursive). Returns the parsed contexts paired
/// with their source paths, sorted by filename for reproducible output.
/// Empty result is an error — `publish-only` cannot proceed without at
/// least one manifest pinning the preserved commit.
fn discover_preserved_contexts(dist: &Path) -> Result<Vec<(PathBuf, PreservedDistContext)>> {
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: reading dist directory {} to discover context manifest(s)",
            dist.display()
        )
    })?;
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "publish-only: reading directory entry under {}",
                dist.display()
            )
        })?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        // Skip the .tmp file the harness's atomic-rename writer may
        // have left behind on a crash mid-write — never represents a
        // committed manifest.
        if name.ends_with(".tmp") {
            continue;
        }
        if name == "context.json" || (name.starts_with("context-") && name.ends_with(".json")) {
            found.push(entry.path());
        }
    }
    if found.is_empty() {
        anyhow::bail!(
            "publish-only: no context.json (or context-<shard>.json) found at {}. \
             Run `anodize check determinism --preserve-dist=<dist-dir>` on a green \
             determinism check first, or use `anodize publish` (no sign step) if \
             you only need the publisher pass.",
            dist.display()
        );
    }
    found.sort();
    let mut out: Vec<(PathBuf, PreservedDistContext)> = Vec::with_capacity(found.len());
    for path in found {
        let parsed = load_preserved_context(&path)?;
        out.push((path, parsed));
    }
    Ok(out)
}

/// Walk `dist/` for every `artifacts.json` and `artifacts-*.json` entry
/// at the dist root (non-recursive). Returns paths sorted by filename
/// for reproducible output. May return an empty vec when neither the
/// legacy nor any sharded manifest is present — callers decide whether
/// that's fatal.
fn discover_artifacts_manifests(dist: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dist).with_context(|| {
        format!(
            "publish-only: reading dist directory {} to discover artifacts manifest(s)",
            dist.display()
        )
    })?;
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "publish-only: reading directory entry under {}",
                dist.display()
            )
        })?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if name == "artifacts.json" || (name.starts_with("artifacts-") && name.ends_with(".json")) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Fold N per-shard `PreservedDistContext` entries into a single view.
/// Semantics:
/// - `artifacts` — concatenated in shard-name (path) order; duplicates
///   are preserved (a duplicate path across shards is a workflow bug
///   worth surfacing downstream rather than silently collapsing).
/// - `targets` — deduped + sorted; the union across all shards.
/// - `version` / `commit` — taken from the first non-empty entry. The
///   later cross-check step verifies every shard's `commit` agrees with
///   the merged value.
fn merge_preserved_contexts(contexts: &[(PathBuf, PreservedDistContext)]) -> PreservedDistContext {
    use std::collections::BTreeSet;
    let mut merged = PreservedDistContext::default();
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for (_, c) in contexts {
        if merged.version.is_empty() && !c.version.is_empty() {
            merged.version = c.version.clone();
        }
        if merged.commit.is_empty() && !c.commit.is_empty() {
            merged.commit = c.commit.clone();
        }
        for t in &c.targets {
            targets.insert(t.clone());
        }
        for a in &c.artifacts {
            merged.artifacts.push(PreservedArtifact {
                name: a.name.clone(),
                path: a.path.clone(),
                sha256: a.sha256.clone(),
                size: a.size,
            });
        }
    }
    merged.targets = targets.into_iter().collect();
    merged
}

fn load_preserved_context(path: &Path) -> Result<PreservedDistContext> {
    if !path.exists() {
        // The recovery hint uses a literal `<dist-dir>` placeholder
        // rather than interpolating `path.parent()` because the parent
        // for a relative `dist/context.json` would be `.` (or empty),
        // producing the misleading "--preserve-dist=." in the error.
        // A literal placeholder is unambiguous.
        anyhow::bail!(
            "publish-only: missing {}. Run `anodize check determinism \
             --preserve-dist=<dist-dir>` on a green determinism check first, or use \
             `anodize publish` (no sign step) if you only need the publisher pass.",
            path.display(),
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("publish-only: read {}", path.display()))?;
    let ctx: PreservedDistContext = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "publish-only: parse {} as PreservedDistContext",
            path.display()
        )
    })?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a closure-returning factory for an env map; tests pass it
    /// to `preflight_credentials` to drive the credential check
    /// deterministically without touching the process env.
    fn env_from(map: HashMap<&str, &str>) -> impl Fn(&str) -> Option<String> {
        let owned: HashMap<String, String> = map
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.get(k).cloned()
    }

    #[test]
    fn load_preserved_context_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_preserved_context(&tmp.path().join("context.json")).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("publish-only: missing"),
            "error should name the publish-only path; got: {msg}"
        );
        assert!(
            msg.contains("--preserve-dist"),
            "error should point at the preserve-dist flag; got: {msg}"
        );
        // M3 regression: the error must use the literal placeholder,
        // not a path.parent() interpolation that would emit "." for
        // relative paths.
        assert!(
            msg.contains("<dist-dir>"),
            "error should use the literal <dist-dir> placeholder; got: {msg}"
        );
    }

    #[test]
    fn load_preserved_context_parses_minimal_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("context.json");
        std::fs::write(
            &path,
            r#"{"artifacts":[{"name":"a.tar.gz","path":"a.tar.gz","sha256":"sha256:abc","size":42}],"targets":["x86_64-unknown-linux-gnu"],"version":"0.1.0","commit":"deadbeefcafe"}"#,
        )
        .unwrap();
        let parsed = load_preserved_context(&path).unwrap();
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.commit, "deadbeefcafe");
        assert_eq!(parsed.targets, vec!["x86_64-unknown-linux-gnu"]);
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0].name, "a.tar.gz");
    }

    #[test]
    fn load_preserved_context_tolerates_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("context.json");
        std::fs::write(&path, r#"{}"#).unwrap();
        let parsed = load_preserved_context(&path).unwrap();
        assert!(parsed.artifacts.is_empty());
        assert!(parsed.targets.is_empty());
        assert_eq!(parsed.version, "");
        assert_eq!(parsed.commit, "");
    }

    #[test]
    fn preflight_credentials_bails_when_token_missing() {
        let err = preflight_credentials(|_| None).unwrap_err();
        assert!(
            format!("{err}").contains("missing release token"),
            "expected missing-token error; got: {err}"
        );
    }

    #[test]
    fn preflight_credentials_bails_when_sign_key_missing() {
        let env = env_from(HashMap::from([("GITHUB_TOKEN", "x")]));
        let err = preflight_credentials(env).unwrap_err();
        assert!(
            format!("{err}").contains("missing production signing key"),
            "expected missing-sign-key error after token set; got: {err}"
        );
    }

    #[test]
    fn preflight_credentials_accepts_token_and_cosign_key() {
        let env = env_from(HashMap::from([("GITHUB_TOKEN", "x"), ("COSIGN_KEY", "y")]));
        preflight_credentials(env).expect("token + cosign should preflight clean");
    }

    #[test]
    fn preflight_credentials_accepts_anodizer_github_token_alias() {
        // The token gate honors both `GITHUB_TOKEN` and
        // `ANODIZER_GITHUB_TOKEN` — verifying the alias avoids a
        // silent regression if someone narrows the constant list.
        let env = env_from(HashMap::from([
            ("ANODIZER_GITHUB_TOKEN", "x"),
            ("GPG_PRIVATE_KEY", "y"),
        ]));
        preflight_credentials(env).expect("anodizer github token + gpg key should preflight clean");
    }

    #[test]
    fn preflight_credentials_rejects_empty_token_value() {
        // Empty-string values count as "missing" (the env-var was
        // exported but never populated). Guards against the case
        // where CI declares the secret but the upstream provider
        // returned nothing.
        let env = env_from(HashMap::from([("GITHUB_TOKEN", ""), ("COSIGN_KEY", "y")]));
        let err = preflight_credentials(env).unwrap_err();
        assert!(
            format!("{err}").contains("missing release token"),
            "empty token must be treated as missing; got: {err}"
        );
    }
}
