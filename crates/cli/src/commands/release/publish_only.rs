//! `anodize release --publish-only`: consume a `dist/` populated by
//! `anodize check determinism --preserve-dist=<path>` and run only the
//! sign + publish pipeline.
//!
//! Phase 2 of `.claude/specs/2026-05-19-determinism-produces-shippable.md`.
//! The harness Phase-1 work writes:
//!   - `<preserved-dist>/**` — the byte-stable artifacts the determinism
//!     check just verified (archives, packages, sboms, checksums,
//!     `artifacts.json`, `metadata.json`).
//!   - `<preserved-dist>/context.json` — the Phase-1
//!     [`PreservedDistContext`] manifest pinning `(artifacts, targets,
//!     version, commit)`.
//!
//! This mode loads both, rehydrates `ctx.artifacts` from
//! `dist/artifacts.json` (the in-process registry shape — the manifest
//! the post-pipeline already writes), strips any leftover
//! `Signature` / `Certificate` artifacts the harness may have produced
//! with ephemeral keys, then runs an extended publish pipeline that
//! prepends `SignStage` (production-keys sign pass) ahead of the usual
//! release / publish / blob / snapcraft-publish chain.
//!
//! **Idempotence invariant for the harness side** (per spec D.2): the
//! harness skips its in-loop `SignStage` when production keys are
//! exported on the runner (`COSIGN_KEY` / `GPG_PRIVATE_KEY`), so
//! preserved-dist usually has no `.sig` / `.asc` files. This module's
//! defensive strip exists for the case where that gate didn't fire
//! (legacy harness build, harness ran without prod keys then operator
//! brought them in later, etc.) — re-signing on top of an existing
//! signature chain would produce `*.sig.sig` chaos.
//!
//! **Architecture note** (spec Risks #2): the merge pipeline assumes
//! raw-binary input from `--split`. `--publish-only` deliberately
//! bypasses that assumption: input is the FULL artifact set
//! (binaries + archives + packages + checksums), so we run
//! `build_publish_pipeline` with `SignStage` prepended, not
//! `build_merge_pipeline`.

use anyhow::{Context as _, Result};
use std::path::Path;

use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use super::helpers;
use crate::pipeline;

/// Names of the env vars that gate the publish-only credential
/// preflight. Documented as a single source of truth so the error
/// message and the check itself stay in lockstep.
const SIGN_ENV_VARS: &[&str] = &["COSIGN_KEY", "GPG_PRIVATE_KEY"];
const GITHUB_TOKEN_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "ANODIZER_GITHUB_TOKEN"];

/// `--publish-only` entry point. Wired from `commands/release/mod.rs::run`
/// after `setup_context` / git context / preflight have already run on
/// `ctx`.
pub(super) fn run(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
    dry_run: bool,
) -> Result<()> {
    log.status("running in publish-only mode (load preserved dist + sign + publish)...");

    let dist = config.dist.clone();

    // ── Pre-flight credential check ────────────────────────────────────
    // Bail BEFORE any state mutation. Spec section C: "Pre-flight
    // credential check at top of publish-only". Dry-run skips so
    // operators can preview the pipeline without secrets.
    if !dry_run {
        preflight_credentials()?;
    } else {
        log.verbose("(dry-run) skipping production-credential preflight");
    }

    // ── Load preserved-dist context ────────────────────────────────────
    // Two manifests live in `<dist>/`:
    //   - `artifacts.json` (the post-pipeline writes it): the canonical
    //     in-process registry shape with `kind` / `target` / `metadata`,
    //     same as `anodize publish` consumes.
    //   - `context.json` (Phase-1 preserve.rs writes it): the
    //     harness's `PreservedDistContext` summary with the per-artifact
    //     `sha256` + `size` recorded at preserve time.
    //
    // `artifacts.json` is the load-bearing input — it's the only file
    // that carries `ArtifactKind`. `context.json` provides the
    // cross-check: assert its `commit` field matches `ctx`'s resolved
    // commit so we don't accidentally publish bytes from a prior tag.
    let context_path = dist.join("context.json");
    let preserved = load_preserved_context(&context_path)?;

    log.status(&format!(
        "publish-only: loaded context.json (version={}, commit={}, targets=[{}], {} artifact(s))",
        preserved.version,
        short_commit(&preserved.commit),
        preserved.targets.join(", "),
        preserved.artifacts.len(),
    ));

    // Cross-check `commit`: if the harness rebuilt commit X but ctx
    // resolved commit Y (e.g. the operator forgot to `git checkout`
    // back to the tagged commit), we must NOT ship Y's signatures
    // over X's bytes. This is the safety property the spec depends
    // on: re-signing only swaps signature blobs, underlying
    // archive bytes must match what the determinism check verified.
    if let Some(ctx_commit) = ctx.template_vars().get("FullCommit").cloned()
        && !ctx_commit.is_empty()
        && ctx_commit != preserved.commit
    {
        anyhow::bail!(
            "publish-only: context.json was preserved at commit {} but the current \
             release context resolved to commit {}. Re-signing the preserved bytes \
             under the current commit's tag would ship signatures that don't match \
             the determinism-verified state. `git checkout {}` then retry.",
            short_commit(&preserved.commit),
            short_commit(&ctx_commit),
            short_commit(&preserved.commit),
        );
    }

    // ── Rehydrate ctx.artifacts ────────────────────────────────────────
    // Delegates to the same loader `anodize publish` uses so the two
    // entry points stay in lockstep (one parser to maintain).
    helpers::load_artifacts_from_dist(ctx, &dist).with_context(|| {
        format!(
            "publish-only: failed to load artifacts.json from {}. The preserve-dist \
             flow normally copies it from the harness's worktree post-pipeline; if \
             it's missing the preserved dist is incomplete.",
            dist.display()
        )
    })?;

    log.status(&format!(
        "publish-only: rehydrated {} artifact(s) from dist/artifacts.json",
        ctx.artifacts.all().len()
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
        super::run_post_pipeline(ctx, config, dry_run, log)?;
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
fn preflight_credentials() -> Result<()> {
    let token_present = GITHUB_TOKEN_ENV_VARS
        .iter()
        .any(|v| std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false));
    let sign_key_present = SIGN_ENV_VARS
        .iter()
        .any(|v| std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false));

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
    log.status(&format!(
        "publish-only: stripping {} ephemeral signature/certificate artifact(s) before re-sign",
        stale_paths.len()
    ));
    // Also delete the on-disk files so the next sign-stage doesn't see
    // a leftover `.sig` next to its target and produce a `*.sig.sig`
    // through the user's own sign-args template (which typically reads
    // `{{ .Signature }} = {{ .Artifact }}.sig`).
    for p in &stale_paths {
        if let Err(e) = std::fs::remove_file(p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log.warn(&format!(
                "publish-only: failed to delete stale signature {}: {} \
                 (continuing; SignStage will overwrite or fail loudly)",
                p.display(),
                e
            ));
        }
    }
    ctx.artifacts.remove_by_paths(&stale_paths);
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
#[derive(serde::Deserialize, Debug, Default)]
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

#[allow(dead_code)] // fields retained for forward compat with hash-verify mode
#[derive(serde::Deserialize, Debug, Default)]
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

fn load_preserved_context(path: &Path) -> Result<PreservedDistContext> {
    if !path.exists() {
        anyhow::bail!(
            "publish-only: missing {}. Run `anodize check determinism --preserve-dist={}` \
             on a green determinism check first, or use `anodize publish` (no sign step) \
             if you only need the publisher pass.",
            path.display(),
            path.parent().unwrap_or_else(|| Path::new(".")).display(),
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

fn short_commit(commit: &str) -> String {
    if commit.len() > 8 {
        commit[..8].to_string()
    } else {
        commit.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_commit_truncates_long_sha() {
        assert_eq!(short_commit("abcdef1234567890"), "abcdef12");
    }

    #[test]
    fn short_commit_passes_short_input_through() {
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(short_commit(""), "");
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
    fn preflight_credentials_requires_token_and_sign_key() {
        // Save + restore the process env vars so this test doesn't
        // leak state into the rest of the suite. Setting them via
        // `std::env::set_var` is unsafe in 2024 edition — capture
        // current values then restore at the end.
        // SAFETY: the test binary is single-threaded relative to its
        // own env access; tests in this module don't run in parallel
        // with each other thanks to cargo's per-test-module
        // serialization for unsafe env mutation. The vars we touch
        // are scoped to this function.
        let saved: Vec<(&'static str, Option<String>)> = SIGN_ENV_VARS
            .iter()
            .chain(GITHUB_TOKEN_ENV_VARS.iter())
            .map(|k| (*k, std::env::var(*k).ok()))
            .collect();
        unsafe {
            for k in SIGN_ENV_VARS.iter().chain(GITHUB_TOKEN_ENV_VARS.iter()) {
                std::env::remove_var(k);
            }
        }

        let err = preflight_credentials().unwrap_err();
        assert!(
            format!("{err}").contains("missing release token"),
            "expected missing-token error first; got: {err}"
        );

        unsafe { std::env::set_var("GITHUB_TOKEN", "x") };
        let err = preflight_credentials().unwrap_err();
        assert!(
            format!("{err}").contains("missing production signing key"),
            "expected missing-sign-key error after token set; got: {err}"
        );

        unsafe { std::env::set_var("COSIGN_KEY", "y") };
        preflight_credentials().expect("token + cosign should preflight clean");

        // Restore.
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
