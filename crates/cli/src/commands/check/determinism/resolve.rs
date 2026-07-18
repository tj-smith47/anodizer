use super::*;

/// Truncate a commit hash to the conventional 7-char "short" form, used
/// in the default `dist/run-<short>/determinism.json` path.
pub(super) fn commit_short(commit: &str) -> String {
    commit.get(..7).unwrap_or(commit).to_string()
}

/// Emit the run-configuration summary beneath the `Checking determinism`
/// header as aligned `kv` detail rows (targets / stages / runs, plus
/// preserve-dist / crate when set). `targets` is `None` when the operator
/// did not pass `--targets` (the harness resolves the project's full target
/// list), rendered as `all (from config)` so the row is never blank.
///
/// This is the only printer of these parameters: callers (including the
/// `anodizer-action` wrapper) must not echo their own copy of the header
/// or the parameter rows.
pub(super) fn emit_run_summary(
    log: &StageLogger,
    targets: Option<&[String]>,
    stages: &[StageId],
    runs: u32,
    preserve_dist: Option<&std::path::Path>,
    crate_name: Option<&str>,
) {
    let targets_value = match targets {
        Some(t) if !t.is_empty() => t.join(", "),
        _ => "all (from config)".to_string(),
    };
    let stages_value = stages
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let preserve_value = preserve_dist.map(|p| p.display().to_string());

    let mut rows: Vec<(&str, &str)> = vec![("targets", targets_value.as_str())];
    rows.push(("stages", stages_value.as_str()));
    let runs_value = runs.to_string();
    rows.push(("runs", runs_value.as_str()));
    if let Some(ref v) = preserve_value {
        rows.push(("preserve-dist", v.as_str()));
    }
    if let Some(name) = crate_name {
        rows.push(("crate", name));
    }
    // Pad every key to the widest EMITTED key so the value column lines up
    // across rows without reserving width for absent optional rows.
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in rows {
        log.kv(key, value, key_width);
    }
}

/// Resolve the harness's `child_snapshot` flag.
///
/// ```text
/// snapshot | no_snapshot | head_at_tag | child_snapshot | reason
/// ---------+-------------+-------------+----------------+--------
///  true    | -           | -           | true           | explicit --snapshot
///  -       | true        | -           | false          | explicit --no-snapshot
///  false   | false       | true        | false          | auto: tagged → release artifacts
///  false   | false       | false       | true           | auto: untagged → snapshot artifacts
/// ```
///
/// Free function so the matrix is unit-testable without forking git.
pub(super) fn resolve_child_snapshot(snapshot: bool, no_snapshot: bool, head_at_tag: bool) -> bool {
    if snapshot {
        true
    } else if no_snapshot {
        false
    } else {
        !head_at_tag
    }
}

/// Derive allow-list entries for signature and keyless-certificate artifacts
/// from the project's `signs:` / `binary_signs:` templates (top-level and
/// per workspace), including the `certificate:` (cosign keyless mode)
/// template alongside `signature:`.
///
/// Signatures are non-reproducible by nature: cosign signs with a random
/// ECDSA nonce, so its bundle/signature bytes differ on every signing of
/// byte-identical input; a keyless certificate is equally per-invocation
/// (Fulcio mints a fresh short-lived cert every sign). `infer_stage_from_path`
/// already classifies the default `.sig` / `.pem` / `.cert` suffixes as the
/// `sign` stage (which the harness auto-allow-lists), but both templates are
/// user-configurable, so a custom suffix (cfgd's `.cosign.bundle`) would
/// fall through to `unknown` and be counted as drift. Deriving the suffixes
/// from config keeps the harness correct for any naming scheme.
///
/// Delegates suffix collection to
/// [`anodizer_core::signature_assets::signature_asset_suffixes`] — the
/// single source of truth also consumed by release verification's digest
/// exemption, so the two stay congruent by construction rather than by two
/// hand-maintained copies of the same walk.
pub(super) fn signature_allowlist_entries_from_config(
    cfg: &anodizer_core::config::Config,
) -> Vec<AllowListEntry> {
    anodizer_core::signature_assets::signature_asset_suffixes(cfg)
        .into_iter()
        .map(|suffix| AllowListEntry {
            reason: format!(
                "signature/certificate artifact ({suffix}): bytes vary by signer \
                 (cosign signs with a random ECDSA nonce / mints a fresh keyless cert); \
                 validate cryptographically via `cosign verify-blob` / `gpg --verify`, \
                 not byte-equality"
            ),
            artifact: format!("*{suffix}"),
        })
        .collect()
}

/// Probe the project's `dockers_v2[*].use` field for a `"podman"` opt-in.
///
/// Returns `Some("podman")` when any `dockers_v2` entry under any crate
/// (or the project-level `defaults.dockers_v2`) sets `use: podman`,
/// `Some("buildx")` when only buildx is configured, and `None` when no
/// `dockers_v2` entries exist. The harness consults the hint to decide
/// whether to short-circuit its `docker buildx`-based reproducibility
/// probe.
pub(super) fn detect_docker_backend_hint(cfg: &anodizer_core::config::Config) -> Option<String> {
    let mut saw_buildx = false;
    let mut iter: Vec<&Option<String>> = Vec::new();
    if let Some(ref defaults) = cfg.defaults
        && let Some(ref v2) = defaults.dockers_v2
    {
        iter.push(&v2.use_backend);
    }
    for c in cfg.crate_universe() {
        if let Some(ref v2s) = c.dockers_v2 {
            for v in v2s {
                iter.push(&v.use_backend);
            }
        }
    }
    for opt in iter {
        match opt.as_deref() {
            Some("podman") => return Some("podman".to_string()),
            Some("buildx") | None => saw_buildx = true,
            Some(_) => {}
        }
    }
    if saw_buildx {
        Some("buildx".to_string())
    } else {
        None
    }
}

/// The crate universe ([`anodizer_core::config::Config::crate_universe`]),
/// optionally scoped to `crate_name`.
///
/// `--crate` scopes to one; a whole-project run (`crate_name == None`) takes
/// all. `defaults.dockers_v2` is materialized onto crates by `apply_defaults`
/// before this runs, so a producer declared only under `defaults:` is seen too.
pub(super) fn crate_universe<'a>(
    cfg: &'a anodizer_core::config::Config,
    crate_name: Option<&'a str>,
) -> impl Iterator<Item = &'a anodizer_core::config::CrateConfig> {
    cfg.crate_universe()
        .into_iter()
        .filter(move |k| crate_name.is_none_or(|n| k.name == n))
}

/// `true` when the crate-under-test DECLARES at least one `dockers_v2` entry —
/// independent of whether any entry later resolves to a buildable image.
///
/// Threaded into [`Harness::docker_declared`] so the harness distinguishes
/// "crate configures no docker image" (quiet clean skip) from "crate declared
/// images but every entry was legitimately skipped in this context" (visible
/// warn-skip that mirrors production). A raw declaration check, deliberately
/// NOT gated on render outcome.
pub(super) fn crate_declares_docker(
    repo_config: Option<&anodizer_core::config::Config>,
    crate_name: Option<&str>,
) -> bool {
    repo_config.is_some_and(|cfg| {
        crate_universe(cfg, crate_name)
            .any(|k| k.dockers_v2.as_ref().is_some_and(|v| !v.is_empty()))
    })
}

/// Resolve the crate-under-test's `dockers_v2` entries into the plain
/// [`ResolvedDockerConfig`] data the harness docker path consumes.
///
/// Mirrors the production `docker` stage's config resolution
/// (`anodizer_stage_docker::prepare_v2_config`) and, critically, its ERROR
/// discipline: every `skip:` evaluation, `dockerfile` render, and `build_args`
/// render is propagated via `?` — never swallowed. Production propagates these
/// (`render_template(&v2_cfg.dockerfile)?`, `is_docker_v2_skipped(...)?`)
/// precisely so a broken template FAILS rather than silently shipping fewer
/// images; the determinism gate must not be laxer, or a mis-rendered image
/// would pass byte-verification it never underwent. A truthy `skip:` or an
/// empty rendered dockerfile is a genuine conditional skip (matching
/// production's `return Ok(())`) and drops the entry; `extra_files` pass through
/// verbatim (production does not template them).
///
/// The `Context` is seeded the SAME way the child `anodize release --snapshot`
/// builds its config-resolution surface for this crate — snapshot/nightly
/// options, process + config `env` ([`helpers::setup_env`]), git + version vars
/// ([`helpers::resolve_git_context`]), the snapshot version suffix
/// ([`release::apply_snapshot_template_vars`]), and time/runtime/metadata vars —
/// so a `dockerfile` / `build_args` template referencing `.Env.*`, `.Version`,
/// `.Commit`, etc. resolves IDENTICALLY to the release build rather than
/// silently rendering empty under an under-seeded context.
pub(super) fn resolve_docker_configs(
    repo_config: Option<&anodizer_core::config::Config>,
    crate_name: Option<&str>,
    child_snapshot: bool,
    log: &StageLogger,
) -> Result<Vec<ResolvedDockerConfig>> {
    let Some(cfg) = repo_config else {
        return Ok(Vec::new());
    };

    // Build the config-resolution Context the child snapshot release would use
    // for this crate, reusing the SAME public helpers the release setup does so
    // the two surfaces cannot drift.
    let opts = anodizer_core::context::ContextOptions {
        snapshot: child_snapshot,
        selected_crates: crate_name.map(|n| vec![n.to_string()]).unwrap_or_default(),
        // The determinism child build skips SIDE_EFFECT_STAGES (incl `release`) and runs
        // credential-less; this parent-side config-resolution Context must evaluate
        // setup_env's release token gate the same way, else a release-mode (tagged-HEAD)
        // probe demands a GitHub token it never uses to resolve dockers_v2.
        skip_stages: anodizer_core::determinism_runner::SIDE_EFFECT_STAGES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        ..Default::default()
    };
    let mut ctx = anodizer_core::context::Context::new(cfg.clone(), opts);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    ctx.populate_metadata_var()?;
    crate::commands::helpers::setup_env(&mut ctx, cfg, log)?;
    crate::commands::helpers::resolve_git_context(&mut ctx, cfg, log)?;
    if child_snapshot {
        crate::commands::release::apply_snapshot_template_vars(&mut ctx, cfg, log)?;
    }

    let mut out: Vec<ResolvedDockerConfig> = Vec::new();
    for krate in crate_universe(cfg, crate_name) {
        let Some(entries) = krate.dockers_v2.as_ref() else {
            continue;
        };
        for (idx, entry) in entries.iter().enumerate() {
            // A truthy `skip:` drops the entry — propagate a render/eval error
            // rather than treating a broken `skip:` template as `false`.
            if anodizer_stage_docker::is_docker_v2_skipped(&entry.skip, &ctx).with_context(
                || format!("dockers_v2[{idx}]: evaluate skip for crate {}", krate.name),
            )? {
                continue;
            }
            // Propagate the dockerfile render error (never swallow) so a broken
            // template fails loudly instead of silently dropping the image.
            let dockerfile = ctx.render_template(&entry.dockerfile).with_context(|| {
                format!(
                    "dockers_v2[{idx}]: render dockerfile path '{}' for crate {}",
                    entry.dockerfile, krate.name
                )
            })?;
            // An empty rendered dockerfile is a genuine conditional skip
            // (matches production's `rendered_dockerfile.trim().is_empty()`).
            if dockerfile.trim().is_empty() {
                continue;
            }
            // Propagate build_arg render errors (never default to empty args,
            // which would build a DIFFERENT image than the release and still
            // report byte-stability).
            //
            // Boundary: build_args referencing `.BaseImage` / `.BaseImageDigest`
            // are NOT rendered here. Production seeds those into the Context
            // post-inspect (a networked `get_base_image` docker inspect) before
            // rendering build_args; the harness deliberately does not run that
            // inspect — it is networked and itself non-deterministic, wrong for
            // a determinism probe. Such args render empty and drop here. This is
            // still a strict improvement over the prior zero-build-args harness,
            // and run-to-run determinism holds because args are resolved once
            // and reused across every rebuild.
            let build_args = anodizer_stage_docker::render_v2_kv_map(
                &mut ctx,
                entry.build_args.as_ref(),
                "build_arg",
            )?;
            out.push(ResolvedDockerConfig {
                dockerfile,
                extra_files: entry.extra_files.clone().unwrap_or_default(),
                build_args,
            });
        }
    }
    Ok(out)
}

/// Fork the determinism docker-stage state on the [`resolve_docker_configs`]
/// outcome and operator intent, returning the harness's `(docker_configs,
/// docker_declared)` pair.
///
/// - `Ok(v)` → carry the resolved configs; `declared` is unchanged.
/// - `Err` under an EXPLICIT request (`--require-tools` / explicit
///   `--stages=docker`) → hard-fail now. Silently skipping a stage the caller
///   asked to byte-verify is false coverage.
/// - `Err` under a HOST-DEFAULT run → warn accurately and reflect the errored
///   resolve as NOT declared (`false`). This keeps the downstream harness state
///   honest: [`Harness::run_docker_stage`]'s `declared && empty` branch emits a
///   "declares dockers_v2 but all entries were legitimately skipped" warn, which
///   would be factually WRONG for an errored resolve (and a redundant second
///   warn). Forcing `declared=false` routes the errored case to the quiet skip,
///   so that legit-skip warn only ever fires for a genuine all-skipped config.
pub(super) fn classify_docker_stage_state(
    resolved: Result<Vec<ResolvedDockerConfig>>,
    declared: bool,
    explicitly_requested: bool,
    log: &StageLogger,
) -> Result<(Vec<ResolvedDockerConfig>, bool)> {
    match resolved {
        Ok(v) => Ok((v, declared)),
        Err(e) if explicitly_requested => Err(e).context(
            "resolving dockers_v2 for the determinism docker stage (--require-tools / \
             explicit --stages=docker)",
        ),
        Err(e) => {
            log.warn(&format!(
                "skipping docker stage — could not resolve dockers_v2 for this run: {e:#}"
            ));
            Ok((Vec::new(), false))
        }
    }
}

/// Resolve the WiX binaries the `msi` stage requires from the loaded config
/// via [`anodizer_stage_msi::required_msi_tools`] — the SAME helper
/// env-preflight consults, so the determinism gate's MSI tool requirement
/// can never drift from the version the build runs (WiX v3 → candle+light,
/// v4 → wix, the Linux path → wixl). Resolution covers all config modes
/// (single / lockstep / per-crate) because `required_msi_tools` iterates the
/// full `crate_universe` and resolves each crate's `msis:` entry under the
/// project Context.
///
/// A missing/unparseable config (`None`) yields an empty list: the gate then
/// treats `msi` as carrying no tool requirement and the real config error
/// surfaces from the pipeline itself.
///
/// `required_msi_tools` renders each entry's `skip:` / `if:` in this bare
/// gate context, which lacks the `--snapshot` child's `.Version` /
/// `IsSnapshot` / `.Env` vars — so a context-dependent skip/if could resolve
/// an entry inactive here yet active in the child, leaving `msi` ungated.
/// Unlike `upx`, that is benign: when no WiX binary is on PATH the stage's
/// version probe falls back to v4 and the child hard-fails at `wix build`
/// spawn (`run_checked`), surfacing the missing tool loudly. There is no
/// silent warn-skip to under-cover, so `msi` needs no conservative
/// over-require (contrast [`resolve_upx_tools`], whose stage warn-skips).
pub(super) fn resolve_msi_tools(
    repo_config: Option<&anodizer_core::config::Config>,
) -> Vec<String> {
    let Some(cfg) = repo_config else {
        return Vec::new();
    };
    let ctx = anodizer_core::context::Context::new(
        cfg.clone(),
        anodizer_core::context::ContextOptions::default(),
    );
    anodizer_stage_msi::required_msi_tools(&ctx)
}

/// Resolve the upx binaries the `upx` stage requires from the loaded config
/// via [`anodizer_stage_upx::required_upx_tools`] — the SAME helper release
/// preflight consults, so the determinism gate's upx requirement can never
/// drift from what the build runs. Each enabled `upx:` entry contributes its
/// `binary` (default `upx`).
///
/// A missing/unparseable config (`None`) yields an empty list: the gate then
/// treats `upx` as carrying no tool requirement and the stage's own runtime
/// guard governs.
///
/// ## Conservative over-require for a templated `enabled:`
///
/// `required_upx_tools` renders each `enabled:` in this bare gate context,
/// which lacks the `--snapshot` child's `.Version` / `IsSnapshot` /
/// `IsHarness` / `.Env` template vars. A context-DEPENDENT `enabled:` can
/// therefore render `false` here yet `true` in the child — and the upx stage
/// WARN-SKIPS a missing binary at default strictness (`UpxStage::run` →
/// `Context::strict_guard`, which only bails under `options.strict`; the
/// determinism child release is not strict). That under-resolution is exactly
/// the silent false coverage `--require-tools` exists to forbid. So any entry
/// whose `enabled:` is a template forces its binary into the requirement set:
/// the gate must never UNDER-require. A literal `enabled: true` / `false` is
/// context-free and stays precisely resolved by the SSOT.
pub(super) fn resolve_upx_tools(
    repo_config: Option<&anodizer_core::config::Config>,
) -> Vec<String> {
    let Some(cfg) = repo_config else {
        return Vec::new();
    };
    let ctx = anodizer_core::context::Context::new(
        cfg.clone(),
        anodizer_core::context::ContextOptions::default(),
    );
    let mut tools = anodizer_stage_upx::required_upx_tools(&ctx);
    for entry in &cfg.upx {
        if entry.enabled.as_ref().is_some_and(|e| e.is_template()) && !tools.contains(&entry.binary)
        {
            tools.push(entry.binary.clone());
        }
    }
    tools
}

/// Read the target project's release version from `<repo>/Cargo.toml`.
///
/// Resolves `[workspace.package].version` first (workspace inheritance,
/// as cfgd uses to share one version across crates), then falls back to
/// `[package].version`. Returns `None` if the manifest is missing,
/// unparseable, or has neither key.
pub(super) fn read_project_version(repo_root: &std::path::Path) -> Option<String> {
    let manifest = repo_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            doc.get("package")
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}
