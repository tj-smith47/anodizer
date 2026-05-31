use std::collections::HashMap;
use std::process::Command;

use anyhow::Context as _;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::UniversalBinaryConfig;
use anodizer_core::context::Context;
use anodizer_core::hooks::run_hooks;
use anodizer_core::util::find_binary;

// ---------------------------------------------------------------------------
// build_universal_binary — run `lipo` to combine arm64 + x86_64 macOS binaries
// ---------------------------------------------------------------------------

/// Resolve the default `ids` filter for a `universal_binaries[]` entry, GR-aligned.
///
/// GR (universalbinary.go:42-44) defaults `unibin.ID` to `ctx.Config.ProjectName`,
/// then `unibin.IDs` to `[unibin.ID]`. Anodizer's per-crate workspace model
/// complicates this — `Binary` artifacts default their `id` metadata to the
/// binary name (= crate name in the common case), not to `project_name`. To
/// match GR's "ids: [<project>]" idiom while keeping multi-crate workspaces
/// working, the resolved default is:
///
///   1. `ub.id` if explicitly set (verbatim);
///   2. `project_name` if any candidate Binary actually carries that id
///      (the GR-typical single-crate case where binary name == project name,
///      or where the user set `build.id: <project_name>`);
///   3. otherwise `crate_name` — anodizer's per-crate fallback for
///      multi-crate workspaces where Binary `id` defaults to the binary name.
///
/// This way a user migrating a GR config that says `ids: [<project>]` sees
/// the expected match in single-crate workspaces, and multi-crate workspaces
/// continue to scope per-crate without forcing every user to set
/// `universal_binaries[].id` explicitly.
fn resolve_default_unibin_ids(
    ub: &UniversalBinaryConfig,
    crate_name: &str,
    ctx: &Context,
) -> Vec<String> {
    if let Some(ref id) = ub.id {
        return vec![id.clone()];
    }
    let project_name = ctx.config.project_name.as_str();
    if !project_name.is_empty() {
        let project_id_seen = ctx
            .artifacts
            .by_kind_and_crate(ArtifactKind::Binary, crate_name)
            .iter()
            .any(|a| {
                a.metadata
                    .get("id")
                    .map(|v| v == project_name)
                    .unwrap_or(false)
            });
        if project_id_seen {
            return vec![project_name.to_string()];
        }
    }
    vec![crate_name.to_string()]
}

/// Resolve the output path that `build_universal_binary` will write to without
/// performing the build. Returns `None` when the source binaries needed for
/// the lipo step are not present, so callers can skip the duplicate-output
/// check on entries that are no-ops on the current platform.
pub(crate) fn project_universal_out_path(
    crate_name: &str,
    ub: &UniversalBinaryConfig,
    ctx: &mut Context,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let log = ctx.logger("build");
    let binaries = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name);
    let default_ids = resolve_default_unibin_ids(ub, crate_name, ctx);
    let effective_ids = ub.ids.clone().unwrap_or(default_ids);
    let filtered: Vec<_> = if !effective_ids.is_empty() {
        binaries
            .into_iter()
            .filter(|a| {
                // GR-aligned: `id`-only filter (mirrors universalbinary.go:255-258
                // `artifact.ByIDs(unibin.IDs...)`). Binary artifacts now always
                // carry an `id` metadata key (see `artifact_meta`), defaulted to
                // the binary name when `build.id` is unset, so the historical
                // `binary`-key fallback is no longer needed.
                a.metadata
                    .get("id")
                    .map(|v| effective_ids.contains(v))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        binaries
    };
    let arm64_present = filtered
        .iter()
        .any(|a| a.target.as_deref() == Some("aarch64-apple-darwin"));
    let x86_64_present = filtered
        .iter()
        .any(|a| a.target.as_deref() == Some("x86_64-apple-darwin"));
    if !arm64_present || !x86_64_present {
        return Ok(None);
    }
    let out_name = if let Some(ref tmpl) = ub.name_template {
        ctx.render_template_strict(tmpl, "universal_binaries name_template", &log)?
    } else {
        ctx.render_template_strict(
            "{{ .ProjectName }}",
            "universal_binaries name_template (default)",
            &log,
        )?
    };
    let ub_dir = ctx.config.dist.join(format!("{}_darwin_all", crate_name));
    Ok(Some(ub_dir.join(&out_name)))
}

pub(crate) fn build_universal_binary(
    crate_name: &str,
    ub: &UniversalBinaryConfig,
    ctx: &mut Context,
    dry_run: bool,
) -> anyhow::Result<()> {
    let log = ctx.logger("build");
    // Collect arm64 and x86_64 macOS binary artifacts for this crate.
    // When `ids` is set, only consider artifacts whose "binary" metadata key (the binary name)
    // is in the list. Build artifacts use "binary" as their identifier, not "id".
    let binaries = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name);

    let default_ids = resolve_default_unibin_ids(ub, crate_name, ctx);
    let effective_ids = ub.ids.clone().unwrap_or(default_ids);

    let filtered: Vec<_> = if !effective_ids.is_empty() {
        binaries
            .into_iter()
            .filter(|a| {
                // GR-aligned: `id`-only filter (universalbinary.go:255-258
                // `artifact.ByIDs(unibin.IDs...)`). `id` is now always populated
                // on Binary artifacts (defaulted to the binary name when
                // `build.id` is unset) so the historical `binary` fallback is
                // unnecessary.
                a.metadata
                    .get("id")
                    .map(|v| effective_ids.contains(v))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        binaries
    };

    let arm64 = filtered
        .iter()
        .find(|a| a.target.as_deref() == Some("aarch64-apple-darwin"));
    let x86_64 = filtered
        .iter()
        .find(|a| a.target.as_deref() == Some("x86_64-apple-darwin"));

    let (arm64_path, x86_64_path) = match (arm64, x86_64) {
        (Some(a), Some(x)) => (a.path.clone(), x.path.clone()),
        _ => {
            // Not an error: universal binaries require both darwin archs, which
            // only exist on macOS builds or in merge mode. On Linux/Windows split
            // builds this skip is expected — not a strict_guard situation.
            log.verbose(&format!(
                "universal_binaries: skipping {crate_name} — \
                 both aarch64-apple-darwin and x86_64-apple-darwin binaries required"
            ));
            return Ok(());
        }
    };

    // `binary_name` is the source binary filename — preserved for the
    // `binary` metadata key (downstream consumers treat it as the binary's
    // on-disk name).
    let binary_name = arm64_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate_name.to_string());

    // Determine output path / name.
    //
    // GoReleaser universalbinary.go:45 — the default `name_template` is
    // `{{ .ProjectName }}`, NOT the source binary filename. We render the
    // default explicitly so `.exe`-suffixed source names and custom
    // `BuildConfig.binary` values do not leak into the universal output.
    let out_name = if let Some(ref tmpl) = ub.name_template {
        ctx.render_template_strict(tmpl, "universal_binaries name_template", &log)?
    } else {
        ctx.render_template_strict(
            "{{ .ProjectName }}",
            "universal_binaries name_template (default)",
            &log,
        )?
    };

    // Place the universal binary in dist/{crate_name}_darwin_all/{name}
    // matching GoReleaser's convention for universal binaries.
    let dist_dir = &ctx.config.dist;
    let ub_dir = dist_dir.join(format!("{}_darwin_all", crate_name));
    let out_path = ub_dir.join(&out_name);

    // Execute pre-hooks if configured
    let template_vars = ctx.template_vars().clone();
    if let Some(ref hooks) = ub.hooks
        && let Some(ref pre) = hooks.pre
    {
        // Universal-binary hooks are not build hooks (no builds[].env applies);
        // GoReleaser's universalbinary runHook injects only ctx.Env + hook.Env.
        run_hooks(
            pre,
            "pre-universal-binary",
            dry_run,
            &log,
            Some(&template_vars),
            None,
        )?;
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        ));
    } else {
        // Check lipo is available — this is an error since the user
        // explicitly configured universal_binaries.
        if !find_binary("lipo") {
            anyhow::bail!(
                "lipo not found but universal_binaries is configured for {crate_name}; \
                 install Xcode command-line tools or ensure lipo is on PATH"
            );
        }

        // Ensure output directory exists
        std::fs::create_dir_all(&ub_dir).with_context(|| {
            format!(
                "failed to create universal binary output dir: {}",
                ub_dir.display()
            )
        })?;

        log.status(&format!(
            "lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        ));

        let output = Command::new("lipo")
            .args([
                "-create",
                "-output",
                &out_path.to_string_lossy(),
                &arm64_path.to_string_lossy(),
                &x86_64_path.to_string_lossy(),
            ])
            .output()
            .with_context(|| format!("failed to spawn lipo for {crate_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("lipo failed for {crate_name}: {}", stderr.trim());
        }
    }

    // Execute post-hooks if configured
    if let Some(ref hooks) = ub.hooks
        && let Some(ref post) = hooks.post
    {
        run_hooks(
            post,
            "post-universal-binary",
            dry_run,
            &log,
            Some(&template_vars),
            None,
        )?;
    }

    // Apply mod_timestamp if configured
    if let Some(ref ts) = ub.mod_timestamp
        && !dry_run
        && out_path.exists()
    {
        let rendered_ts = ctx
            .render_template(ts)
            .with_context(|| format!("build: render universal mod_timestamp template '{ts}'"))?;
        let mtime = anodizer_core::util::parse_mod_timestamp(&rendered_ts)?;
        anodizer_core::util::set_file_mtime(&out_path, mtime)?;
        log.verbose(&format!(
            "applied mod_timestamp={rendered_ts} to {}",
            out_path.display()
        ));
    }

    // Register the universal binary artifact with UniversalBinary kind.
    // Set `replaces` metadata for OnlyReplacingUnibins publisher filter:
    // true = this universal binary supersedes per-arch variants in publishers.
    let replaces = ub.replace == Some(true);

    // GR-aligned (universalbinary.go:236-239): copy the entire `Extra` map
    // from the first source binary, then overwrite universal-specific keys.
    // The previous 4-key whitelist (`dynamically_linked`, `abi`, `libc`, `id`)
    // silently dropped any other metadata stage-build emits today or might
    // emit tomorrow (e.g. `DynamicallyLinked`, `amd64_variant`, future keys),
    // so anodizer was losing fidelity GR preserves.
    //
    // Caveat for downstream consumers: any metadata key inherited here
    // (notably `DynamicallyLinked`) reflects the FIRST source arch only —
    // `arm64` if both are present, otherwise `x86_64`. A lipo'd binary is a
    // fat Mach-O carrying both slices, so a value derived from one arch's
    // ELF/Mach-O probe does not necessarily describe the other slice.
    // Consumers reading per-arch facts off a `UniversalBinary` artifact
    // should treat the value as best-effort and prefer probing the
    // underlying source `Binary` artifacts when the answer must be exact.
    let mut metadata: HashMap<String, String> = HashMap::new();
    let first_source = arm64.or(x86_64);
    if let Some(src) = first_source {
        metadata.extend(src.metadata.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    // Universal-specific keys (override any copied values).
    metadata.insert("binary".to_string(), binary_name);
    metadata.insert("universal".to_string(), "true".to_string());
    metadata.insert("replaces".to_string(), replaces.to_string());
    // Universal binary's own id, if configured (otherwise the inherited
    // source-binary `id` remains, matching GR's `extra[ExtraID] = unibin.ID`
    // override at universalbinary.go:239 — when `unibin.ID` is empty, the
    // first source's id passes through unchanged).
    if let Some(ref id) = ub.id {
        metadata.insert("id".to_string(), id.clone());
    }

    let universal_name = out_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UniversalBinary,
        name: universal_name,
        path: out_path,
        target: Some("darwin-universal".to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });

    // When `replace` is true, remove the source arm64/x86_64 artifacts from
    // the registry so downstream stages do not publish them alongside the
    // universal binary.
    if ub.replace == Some(true) {
        ctx.artifacts.remove_by_paths(&[arm64_path, x86_64_path]);
    }

    Ok(())
}
