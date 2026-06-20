//! Subprocess runner for the determinism harness.
//!
//! Allow-listed entry-point for `Command::new` in core. The determinism
//! harness in `crates/cli/src/determinism_harness.rs` is forbidden
//! from spawning processes directly per the module-boundary rule, so
//! this module owns the `anodize release --snapshot --skip=...`
//! invocation that drives each from-clean rebuild.
//!
//! Why a separate module: `Command::new` is an authorization boundary
//! (write-to-disk, network, env exfiltration); concentrating the
//! harness's one call site here keeps the security-relevant surface
//! reviewable.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Stage names the determinism harness must NOT run.
///
/// Single source of truth for the `--skip=...` list passed to the child
/// `anodize release --snapshot` invocation. Every entry here is a stage
/// in `crates/cli/src/pipeline/builders.rs::build_release_pipeline` that either:
///
/// - touches upstream (uploads, API calls, push, announce), OR
/// - mutates host state outside `<worktree>/dist` (docker daemon, kms),
///
/// i.e. a "side-effect" stage that has no place in a hermetic regression
/// rebuild. Adding a future side-effect stage to the release pipeline
/// MUST add its stage name here too — otherwise the harness will fire it
/// from inside the supposedly-hermetic build.
///
/// Order mirrors the position in `build_release_pipeline` so reviewers
/// scanning both files can pattern-match. Listed exhaustively (no
/// `starts_with` / glob matching) so a new stage with a similar name
/// (e.g. `docker-extra`) doesn't accidentally inherit the skip.
pub const SIDE_EFFECT_STAGES: &[&str] = &[
    // Publish phase — upstream side effects.
    "release",
    "docker",
    "docker-sign",
    "publish",
    "blob",
    "snapcraft-publish",
    "announce",
    // Post-publish verification — issues live GitHub API calls and (when
    // configured) docker spawns; never run inside a hermetic regression rebuild.
    "verify-release",
];

/// Comma-join [`SIDE_EFFECT_STAGES`] plus an `extra` list for use as
/// the `--skip=<list>` CLI argument value. Order-preserving and
/// duplicate-free: every entry from [`SIDE_EFFECT_STAGES`] comes first
/// (in declared order), then each `extra` entry that hasn't already been
/// seen. Kept as a function (not a const) because Rust can't
/// const-evaluate `[&str]::join`.
///
/// The `extra` argument is the harness's "complement set": every stage
/// the operator did NOT request via `--stages=` AND that doesn't belong
/// to the preamble preserve set (`validate` / `before` / `changelog` /
/// `templatefiles`). Skipping them in the child release subprocess
/// matches the spec's promise that `anodize check determinism
/// --stages=<list>` only exercises (and validates) the named stages —
/// previously the child still ran the full pipeline, attempting nfpm /
/// nsis / dmg / etc. on shards that have no business running them.
pub fn compute_skip_arg(extra: &[&str]) -> String {
    let mut merged: Vec<&str> = Vec::with_capacity(SIDE_EFFECT_STAGES.len() + extra.len());
    for &name in SIDE_EFFECT_STAGES {
        if !merged.contains(&name) {
            merged.push(name);
        }
    }
    for &name in extra {
        if !merged.contains(&name) {
            merged.push(name);
        }
    }
    format!("--skip={}", merged.join(","))
}

/// Invoke the running `anodize` binary against `worktree_path` with the
/// supplied isolated env.
///
/// Pinning args:
/// - `release` — drives the full build-side pipeline.
/// - `--snapshot` (when `snapshot` is `true`) — disables tag-cutting and
///   tells stages to use the pre-resolved SDE. The release workflow
///   passes `false` on tag-push runs so produce-stages emit artifacts
///   named with the actual release version (no `-SNAPSHOT-<sha>` suffix)
///   that the publish-only path can ship directly.
/// - `--skip=<SIDE_EFFECT_STAGES + extra_skip>` — strips every
///   side-effect-producing stage AND every non-requested produce-stage
///   (the harness's complement set). Doubling N is safe in any env
///   because of this skip list.
/// - `--no-preflight` — always. The replica runs in a deliberately
///   credential-less env; see [`build_subprocess_command`].
/// - `--rollback none` — always. The hermetic harness has no published
///   release to undo, so the child's default rollback (which would probe
///   GitHub without a token) is disabled by construction.
/// - `--targets=<csv>` (when `targets` is `Some`) — restricts the
///   rebuild to a subset of configured triples. The sharded
///   `release.yml` matrix passes this so each runner only validates
///   the targets it can natively build (cross-compile to Apple /
///   Windows from a Linux runner would otherwise fail at link time).
///
/// The `extra_skip` slice carries the harness's complement set: stages
/// the operator did NOT name via `--stages=` (minus the preamble
/// preserve set). Merged with [`SIDE_EFFECT_STAGES`] via
/// [`compute_skip_arg`]; the harness in
/// `crates/cli/src/determinism_harness.rs` is the canonical caller and
/// computes the set from `anodizer_core::context::VALID_RELEASE_SKIPS`.
/// Pass `&[]` to keep the legacy "side-effect stages only" behavior.
///
/// The child env is fully replaced (`env_clear` then re-populate) so
/// host env vars cannot leak through and perturb the build. Caller
/// (the harness) constructs the env map.
pub fn run_build_pipeline_subprocess(spec: &ChildInvocation<'_>) -> Result<()> {
    let mut cmd = build_subprocess_command(spec);
    tracing::debug!(
        args = ?cmd.get_args(),
        worktree = %spec.worktree_path.display(),
        "spawning anodize release child for determinism harness",
    );
    let status = cmd
        .status()
        .context("spawning anodize release for determinism harness")?;
    anyhow::ensure!(
        status.success(),
        "harness build pipeline failed in worktree {} (exit {:?})",
        spec.worktree_path.display(),
        status.code()
    );
    Ok(())
}

/// Invocation knobs for the child `anodize release` subprocess, grouped
/// so the spawn surface takes one spec instead of a positional argument
/// list that grows with every new knob.
pub struct ChildInvocation<'a> {
    /// Path to the running `anodize` binary (see
    /// [`current_anodize_binary`]).
    pub anodize_binary: &'a Path,
    /// Hermetic worktree the child builds in (`current_dir`).
    pub worktree_path: &'a Path,
    /// Fully-replacing child env map (`env_clear` + re-populate);
    /// constructed by the harness.
    pub env: &'a HashMap<String, String>,
    /// `--targets=<csv>` restriction; `None` validates every configured
    /// target.
    pub targets: Option<&'a [String]>,
    /// The harness's complement skip set, merged with
    /// [`SIDE_EFFECT_STAGES`] via [`compute_skip_arg`]. Pass `&[]` for
    /// the legacy "side-effect stages only" behavior.
    pub extra_skip: &'a [String],
    /// Whether the child gets `--snapshot`. The release workflow passes
    /// `false` on tag-push runs so artifacts carry the real version.
    pub snapshot: bool,
    /// `--crate=<name>` scoping for per-crate shards; `None` builds the
    /// workspace default.
    pub crate_name: Option<&'a str>,
    /// Operator verbosity, forwarded as `--quiet` / `--verbose` /
    /// `--debug` so the child's inherited stderr honors the same
    /// contract as the harness's own logger.
    pub verbosity: crate::log::Verbosity,
}

/// Build the [`Command`] the harness will spawn. Split out from
/// [`run_build_pipeline_subprocess`] so unit tests can inspect the
/// constructed argv (`cmd.get_args()`) without shelling out — the
/// alternative is to ship a real `anodize` binary into the test harness.
fn build_subprocess_command(spec: &ChildInvocation<'_>) -> Command {
    let ChildInvocation {
        anodize_binary,
        worktree_path,
        env,
        targets,
        extra_skip,
        snapshot,
        crate_name,
        verbosity,
    } = *spec;
    let mut cmd = Command::new(anodize_binary);
    let extra_refs: Vec<&str> = extra_skip.iter().map(String::as_str).collect();
    cmd.arg("release");
    if snapshot {
        cmd.arg("--snapshot");
    }
    // The harness is hermetic: it skips the release stage and runs
    // credential-less, so there is never a published release to undo. The
    // child's default `on_failure=rollback` would otherwise probe GitHub
    // (`get_release_by_tag`) with no token on any stage failure and emit a
    // confusing "set the GH_TOKEN environment variable" warning. Force the
    // no-op rollback mode so a harness stage failure surfaces plainly.
    cmd.arg("--rollback").arg("none");
    // The child's stderr is inherited into the harness's own stream, so
    // the operator's verbosity choice must extend to the child — a
    // `check determinism -q` whose children still print every section
    // would make the flag meaningless.
    match verbosity {
        crate::log::Verbosity::Quiet => {
            cmd.arg("--quiet");
        }
        crate::log::Verbosity::Verbose => {
            cmd.arg("--verbose");
        }
        crate::log::Verbosity::Debug => {
            cmd.arg("--debug");
        }
        crate::log::Verbosity::Normal => {}
    }
    cmd.arg(compute_skip_arg(&extra_refs));
    // The replica pipeline runs in a deliberately credential-less hermetic
    // env (env_clear + identity-only re-population): its run paths skip
    // gracefully when keys/tools are absent, nothing publishes (see the
    // skip list above), and signature outputs are excluded from
    // byte-comparison. The config-derived env preflight would therefore
    // reject exactly the environment the harness is designed to run in —
    // disable it for the child by construction. Real release entrypoints
    // are unaffected; preflight guards them as before.
    cmd.arg("--no-preflight");
    if let Some(list) = targets
        && !list.is_empty()
    {
        cmd.arg(format!("--targets={}", list.join(",")));
    }
    // Scope the child build to the same crate the harness is preserving for.
    // Without it a workspace build defaults to its primary crate, so a
    // per-crate shard would rebuild (and preserve) the wrong member's
    // artifacts — e.g. a library's source archive in place of a binary
    // crate's compiled binaries, leaving publish-only with no binary to
    // ship or stage into a docker context.
    if let Some(name) = crate_name {
        cmd.arg(format!("--crate={name}"));
    }
    cmd.current_dir(worktree_path);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    // anodizer's stdout is a machine-readable data channel (GHA step
    // outputs, JSON payloads); the harness consumes none of the child's —
    // it reads artifacts from disk and gates on the exit status alone. Null
    // the child's stdout so its data channel never pollutes the harness's
    // own stdout. The child's *stderr* (its logger's status/verbose lines)
    // stays inherited so the operator's verbosity choice, forwarded above,
    // still surfaces the inner run's progress.
    cmd.stdout(Stdio::null());
    // The child's stderr is inherited, so its lines interleave straight
    // into the harness's stream. Export the parent's nesting depth (+2)
    // so the child's section headers render beneath the harness's
    // `• run N of M` bullet instead of flush-left: the bullet itself
    // sits one section level plus a body indent under the harness
    // header, and the child's sections belong one further level in.
    // Set AFTER the hermetic env map so the map cannot clobber it.
    cmd.env(
        crate::log::LOG_DEPTH_ENV,
        (crate::log::current_depth() + 2).to_string(),
    );
    cmd
}

/// Resolve the path of the currently-running `anodize` binary. Thin
/// wrapper over [`std::env::current_exe`] kept here so the harness side
/// doesn't have to touch `std::env` for binary resolution.
pub fn current_anodize_binary() -> Result<PathBuf> {
    std::env::current_exe().context("locating the currently-running anodize binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_binary_resolves_to_a_real_file() {
        // In test context, `current_exe` returns the test runner; the
        // path is just expected to be readable.
        let p = current_anodize_binary().unwrap();
        assert!(p.exists(), "current_exe should point at a real file");
    }

    #[test]
    fn run_build_pipeline_subprocess_fails_when_binary_missing() {
        let env = HashMap::new();
        let worktree = std::env::temp_dir();
        let bogus = PathBuf::from("/nonexistent/anodize-binary-for-tests");
        let res = run_build_pipeline_subprocess(&ChildInvocation {
            anodize_binary: &bogus,
            worktree_path: &worktree,
            env: &env,
            targets: None,
            extra_skip: &[],
            snapshot: true,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        assert!(
            res.is_err(),
            "missing binary should surface as an error, not a panic"
        );
    }

    /// Argv shape sanity: no `--targets` flag when the harness passes
    /// `None` (legacy single-runner path validates every configured
    /// target).
    #[test]
    fn subprocess_command_omits_targets_when_none() {
        let env = HashMap::new();
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: None,
            extra_skip: &[],
            snapshot: true,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().expect("ascii")).collect();
        assert!(
            args.iter().all(|a| !a.starts_with("--targets")),
            "expected no --targets argument; got {args:?}"
        );
        // Sanity: --snapshot + --skip=... still present.
        assert!(
            args.contains(&"--snapshot"),
            "argv missing --snapshot: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--skip=")),
            "argv missing --skip=...: {args:?}"
        );
    }

    /// When the harness restricts targets, the child subprocess gets
    /// the same restriction as a single `--targets=<csv>` argument.
    /// Sharded release.yml depends on this — each OS shard must only
    /// rebuild its own native targets.
    #[test]
    fn subprocess_command_propagates_targets_csv() {
        let env = HashMap::new();
        let triples = vec![
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ];
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: Some(&triples),
            extra_skip: &[],
            snapshot: true,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_str().expect("ascii").to_string())
            .collect();
        assert!(
            args.iter()
                .any(|a| a == "--targets=x86_64-apple-darwin,aarch64-apple-darwin"),
            "expected joined --targets= argument; got {args:?}"
        );
    }

    /// Empty-slice short-circuit: an explicit `Some(&[])` should NOT
    /// produce `--targets=` (which would parse to "all-empty CSV" and
    /// fail downstream). The harness is expected to pass `None` when it
    /// has nothing to filter on; this guards against a future caller
    /// passing an empty `Vec` by accident.
    #[test]
    fn subprocess_command_drops_targets_when_list_is_empty() {
        let env = HashMap::new();
        let empty: Vec<String> = Vec::new();
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: Some(&empty),
            extra_skip: &[],
            snapshot: true,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_str().expect("ascii").to_string())
            .collect();
        assert!(
            args.iter().all(|a| !a.starts_with("--targets")),
            "empty target slice should omit --targets entirely; got {args:?}"
        );
    }

    /// The child release subprocess MUST carry `--no-preflight` in every
    /// mode — snapshot children skip the env preflight via the snapshot
    /// gate anyway, but non-snapshot children (tag-push determinism runs,
    /// where the workflow passes `--no-snapshot` so artifacts carry the
    /// real version) would otherwise run the config-derived env preflight
    /// inside the credential-less worktree and abort the replica build on
    /// missing secrets/tools the run paths handle gracefully.
    #[test]
    fn subprocess_command_always_disables_env_preflight() {
        let env = HashMap::new();
        for snapshot in [true, false] {
            let cmd = build_subprocess_command(&ChildInvocation {
                anodize_binary: &PathBuf::from("/usr/bin/anodize"),
                worktree_path: &std::env::temp_dir(),
                env: &env,
                targets: None,
                extra_skip: &[],
                snapshot,
                crate_name: None,
                verbosity: crate::log::Verbosity::Normal,
            });
            let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().expect("ascii")).collect();
            assert!(
                args.contains(&"--no-preflight"),
                "child argv (snapshot={snapshot}) must always carry --no-preflight; got {args:?}"
            );
        }
    }

    /// The child release subprocess MUST disable rollback in every mode.
    /// The harness is hermetic (release stage skipped, no credentials), so
    /// there is never a published release to undo; the child's default
    /// `on_failure=rollback` would otherwise probe GitHub without a token
    /// on any stage failure and emit a confusing GH_TOKEN warning. Assert
    /// `--rollback` is present and immediately followed by `none`.
    #[test]
    fn subprocess_command_always_disables_rollback() {
        let env = HashMap::new();
        for snapshot in [true, false] {
            let cmd = build_subprocess_command(&ChildInvocation {
                anodize_binary: &PathBuf::from("/usr/bin/anodize"),
                worktree_path: &std::env::temp_dir(),
                env: &env,
                targets: None,
                extra_skip: &[],
                snapshot,
                crate_name: None,
                verbosity: crate::log::Verbosity::Normal,
            });
            let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().expect("ascii")).collect();
            let pos = args
                .iter()
                .position(|a| *a == "--rollback")
                .unwrap_or_else(|| {
                    panic!("child argv (snapshot={snapshot}) must carry --rollback; got {args:?}")
                });
            assert_eq!(
                args.get(pos + 1),
                Some(&"none"),
                "child argv (snapshot={snapshot}) must pass `--rollback none`; got {args:?}"
            );
        }
    }

    /// The child env must ALWAYS carry the log-depth var so the child's
    /// interleaved stderr nests beneath the harness's `• run N of M`
    /// bullet — and it must survive the hermetic `env_clear` +
    /// re-population (it is set after the map is applied, so the map
    /// cannot clobber it).
    #[test]
    fn subprocess_command_always_exports_log_depth() {
        // A hermetic map that tries to clobber the var: the explicit
        // post-map set must win.
        let mut env = HashMap::new();
        env.insert(crate::log::LOG_DEPTH_ENV.to_string(), "99".to_string());
        for snapshot in [true, false] {
            let cmd = build_subprocess_command(&ChildInvocation {
                anodize_binary: &PathBuf::from("/usr/bin/anodize"),
                worktree_path: &std::env::temp_dir(),
                env: &env,
                targets: None,
                extra_skip: &[],
                snapshot,
                crate_name: None,
                verbosity: crate::log::Verbosity::Normal,
            });
            let depth = cmd
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(crate::log::LOG_DEPTH_ENV))
                .and_then(|(_, v)| v)
                .and_then(|v| v.to_str())
                .map(str::to_string);
            // Pin presence + the `+2` floor rather than an exact value:
            // SECTION_DEPTH is process-global and sibling tests open
            // sections concurrently, so the exact depth at build time is
            // not stable under a parallel test runner.
            let parsed: usize = depth
                .as_deref()
                .unwrap_or_else(|| {
                    panic!(
                        "child env (snapshot={snapshot}) must carry {}",
                        crate::log::LOG_DEPTH_ENV
                    )
                })
                .parse()
                .expect("depth var must be numeric");
            assert!(
                parsed >= 2,
                "depth must be parent depth + 2 (>= 2); got {parsed}"
            );
        }
    }

    /// `snapshot=false` MUST drop `--snapshot` from the argv so the
    /// child release subprocess uses the real release version instead
    /// of a `-SNAPSHOT-<sha>` suffix. The release workflow relies on
    /// this for tag-push runs.
    #[test]
    fn subprocess_command_drops_snapshot_when_disabled() {
        let env = HashMap::new();
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: None,
            extra_skip: &[],
            snapshot: false,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().expect("ascii")).collect();
        assert!(
            !args.contains(&"--snapshot"),
            "snapshot=false should drop --snapshot; got {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--skip=")),
            "argv still needs --skip=...: {args:?}"
        );
        assert_eq!(args[0], "release", "argv must lead with `release`");
    }

    /// A per-crate determinism shard MUST scope the child build to its
    /// crate, else a workspace build defaults to the primary member and
    /// the shard preserves the wrong crate's artifacts (a library's source
    /// archive in place of a binary crate's binaries), starving publish-only
    /// of the binaries docker and the binary publishers need.
    #[test]
    fn subprocess_command_scopes_to_crate_when_named() {
        let env = HashMap::new();
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: None,
            extra_skip: &[],
            snapshot: true,
            crate_name: Some("cfgd"),
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_str().expect("ascii").to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "--crate=cfgd"),
            "expected --crate=cfgd to scope the child build; got {args:?}"
        );
    }

    /// `None` (a single-crate / non-workspace project) must NOT emit
    /// `--crate`, so the default whole-project build is preserved.
    #[test]
    fn subprocess_command_omits_crate_when_none() {
        let env = HashMap::new();
        let cmd = build_subprocess_command(&ChildInvocation {
            anodize_binary: &PathBuf::from("/usr/bin/anodize"),
            worktree_path: &std::env::temp_dir(),
            env: &env,
            targets: None,
            extra_skip: &[],
            snapshot: true,
            crate_name: None,
            verbosity: crate::log::Verbosity::Normal,
        });
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_str().expect("ascii").to_string())
            .collect();
        assert!(
            args.iter().all(|a| !a.starts_with("--crate")),
            "no crate named: --crate must be omitted; got {args:?}"
        );
    }

    /// Operator verbosity must reach the child argv: each non-Normal
    /// [`crate::log::Verbosity`] maps to exactly one flag, and Normal
    /// maps to none (the child's own default). The child's stderr is
    /// inherited into the parent stream, so a dropped flag would leave
    /// `-q` runs loud and `--debug` runs mute inside the harness.
    #[test]
    fn subprocess_command_forwards_verbosity_flag() {
        let env = HashMap::new();
        let argv_for = |verbosity: crate::log::Verbosity| -> Vec<String> {
            let cmd = build_subprocess_command(&ChildInvocation {
                anodize_binary: &PathBuf::from("/usr/bin/anodize"),
                worktree_path: &std::env::temp_dir(),
                env: &env,
                targets: None,
                extra_skip: &[],
                snapshot: true,
                crate_name: None,
                verbosity,
            });
            cmd.get_args()
                .map(|s| s.to_str().expect("ascii").to_string())
                .collect()
        };
        let verbosity_flags = ["--quiet", "--verbose", "--debug"];
        for (verbosity, expected) in [
            (crate::log::Verbosity::Quiet, Some("--quiet")),
            (crate::log::Verbosity::Verbose, Some("--verbose")),
            (crate::log::Verbosity::Debug, Some("--debug")),
            (crate::log::Verbosity::Normal, None),
        ] {
            let args = argv_for(verbosity);
            let present: Vec<&String> = args
                .iter()
                .filter(|a| verbosity_flags.contains(&a.as_str()))
                .collect();
            match expected {
                Some(flag) => assert_eq!(
                    present,
                    vec![flag],
                    "{verbosity:?} must forward exactly {flag}; got {args:?}"
                ),
                None => assert!(
                    present.is_empty(),
                    "Normal must forward no verbosity flag; got {args:?}"
                ),
            }
        }
    }

    #[test]
    fn side_effect_stages_covers_every_known_publish_side_effect() {
        // Regression guard: if a future pipeline edit adds a side-effect
        // stage and forgets to register it here, this test surfaces the
        // omission. Add the new stage to SIDE_EFFECT_STAGES (and update
        // this list) once the new entry is confirmed to belong in the
        // skip set.
        let expected = [
            "release",
            "docker",
            "docker-sign",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
            "verify-release",
        ];
        for name in expected {
            assert!(
                SIDE_EFFECT_STAGES.contains(&name),
                "SIDE_EFFECT_STAGES missing known publish-side stage `{name}`"
            );
        }
    }

    #[test]
    fn compute_skip_arg_starts_with_skip_flag() {
        // I8 fix shape: harness still uses --skip=<list> (the conservative
        // path; --only=<list> would require a new CLI flag). Guard against
        // a future refactor accidentally flipping to a different prefix.
        let arg = compute_skip_arg(&[]);
        assert!(
            arg.starts_with("--skip="),
            "expected --skip= prefix, got `{arg}`"
        );
        // And the joined list is non-empty.
        assert!(arg.len() > "--skip=".len(), "skip list must not be empty");
    }

    #[test]
    fn compute_skip_arg_round_trips_through_comma_join() {
        let arg = compute_skip_arg(&[]);
        let list = arg
            .trim_start_matches("--skip=")
            .split(',')
            .collect::<Vec<_>>();
        assert_eq!(list.len(), SIDE_EFFECT_STAGES.len());
        for (a, b) in list.iter().zip(SIDE_EFFECT_STAGES.iter()) {
            assert_eq!(a, b);
        }
    }

    /// `compute_skip_arg` MUST merge `SIDE_EFFECT_STAGES` with the
    /// harness's complement set — otherwise the child release subprocess
    /// runs produce-stages like `nfpm` / `nsis` / `dmg` on shards that
    /// have no business running them, and the run dies with `No such
    /// file or directory`. The harness fix in
    /// `crates/cli/src/determinism_harness.rs` relies on this merge.
    #[test]
    fn compute_skip_arg_includes_side_effects_and_extra() {
        let extra = ["nfpm".to_string(), "msi".to_string(), "dmg".to_string()];
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let arg = compute_skip_arg(&extra_refs);
        let list: Vec<&str> = arg.trim_start_matches("--skip=").split(',').collect();
        for &name in SIDE_EFFECT_STAGES {
            assert!(
                list.contains(&name),
                "merged skip list missing side-effect stage `{name}`: {list:?}"
            );
        }
        for name in ["nfpm", "msi", "dmg"] {
            assert!(
                list.contains(&name),
                "merged skip list missing extra stage `{name}`: {list:?}"
            );
        }
    }

    /// Overlap is a real scenario — the harness's complement set is
    /// computed against `VALID_RELEASE_SKIPS`, which contains the same
    /// `release` / `publish` / `announce` names as `SIDE_EFFECT_STAGES`.
    /// `compute_skip_arg` MUST de-dupe so the final argv isn't bloated
    /// and CLI validation doesn't choke on a repeated token.
    #[test]
    fn compute_skip_arg_dedupes_overlap() {
        // Pass a SIDE_EFFECT_STAGES member through `extra` and confirm it
        // appears exactly once in the merged list.
        let extra = ["release".to_string(), "nfpm".to_string()];
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let arg = compute_skip_arg(&extra_refs);
        let list: Vec<&str> = arg.trim_start_matches("--skip=").split(',').collect();
        let release_count = list.iter().filter(|&&s| s == "release").count();
        assert_eq!(
            release_count, 1,
            "expected `release` exactly once in merged skip list, got {release_count} in {list:?}"
        );
        // And nfpm did come through.
        assert!(
            list.contains(&"nfpm"),
            "merged list missing extra entry `nfpm`: {list:?}"
        );
    }

    /// Every name the harness shovels into `--skip=...` MUST be accepted
    /// by the release CLI's skip validator. Surfaced by the
    /// drift-injection integration test when `docker-sign`
    /// was present in [`SIDE_EFFECT_STAGES`] but missing from
    /// [`crate::context::VALID_RELEASE_SKIPS`] — the harness's child
    /// subprocess bombed with `invalid --skip value(s): docker-sign`. This
    /// pure-cross-check unit test catches the drift in milliseconds so a
    /// future addition to either list flags the gap immediately.
    #[test]
    fn side_effect_stages_are_all_valid_release_skip_values() {
        use crate::context::VALID_RELEASE_SKIPS;
        for &name in SIDE_EFFECT_STAGES {
            assert!(
                VALID_RELEASE_SKIPS.contains(&name),
                "SIDE_EFFECT_STAGES contains `{name}` but VALID_RELEASE_SKIPS does not — \
                 the harness would fail at `anodize release --skip=<list>` invocation. \
                 Add `{name}` to VALID_RELEASE_SKIPS in crates/core/src/context.rs."
            );
        }
    }
}
