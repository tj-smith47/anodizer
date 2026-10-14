use std::collections::HashMap;
use std::process::Command;

use anyhow::Context as _;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::UniversalBinaryConfig;
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::tool_detect::on_path;

// build_universal_binary — run `lipo` to combine arm64 + x86_64 macOS binaries

/// Resolve the default `ids` filter for a `universal_binaries[]` entry.
///
/// `Binary` artifacts default their `id` metadata to the binary name (the
/// crate name in the common case), not to `project_name`, so the default
/// has to serve both the single-crate `ids: [<project>]` idiom and a
/// multi-crate workspace. The resolved default is:
///
///   1. `ub.id` if explicitly set (verbatim);
///   2. `project_name` if any candidate Binary actually carries that id
///      (the typical single-crate case where binary name == project name,
///      or where the user set `build.id: <project_name>`);
///   3. otherwise `crate_name` — anodizer's per-crate fallback for
///      multi-crate workspaces where Binary `id` defaults to the binary name.
///
/// This way a config that says `ids: [<project>]` sees
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

/// The crate's `Binary` artifacts that `ub.ids` (or its resolved default)
/// selects. The filter reads the `id` metadata key only: `artifact_meta`
/// always sets it, defaulting to the binary name when `builds[].id` is unset.
fn selected_binaries<'a>(
    ctx: &'a Context,
    crate_name: &str,
    ub: &UniversalBinaryConfig,
) -> Vec<&'a Artifact> {
    let binaries = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name);
    let default_ids = resolve_default_unibin_ids(ub, crate_name, ctx);
    let effective_ids = ub.ids.clone().unwrap_or(default_ids);
    if effective_ids.is_empty() {
        return binaries;
    }
    binaries
        .into_iter()
        .filter(|a| {
            a.metadata
                .get("id")
                .is_some_and(|v| effective_ids.contains(v))
        })
        .collect()
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
    let filtered = selected_binaries(ctx, crate_name, ub);
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

/// The one binary of `target` among the selected slices, or `None` when
/// there is none. Two binaries on one target are two slices of one CPU type
/// (a baseline and a `v3` amd64 build of the same binary), and `lipo` can
/// hold only one of them; taking the first would merge whichever was built
/// first and drop the other without a word.
fn one_slice_per_target<'a>(
    binaries: &[&'a Artifact],
    target: &str,
    crate_name: &str,
) -> anyhow::Result<Option<&'a Artifact>> {
    let mut slices = binaries
        .iter()
        .copied()
        .filter(|a| a.target.as_deref() == Some(target));
    let first = slices.next();
    if let (Some(first), Some(second)) = (first, slices.next()) {
        anyhow::bail!(
            "universal binary for {crate_name}: duplicate darwin binary slice for {target}: \
             {} and {}; give each build its own `builds[].id` and narrow \
             `universal_binaries[].ids` to one of them",
            describe_slice(first),
            describe_slice(second)
        );
    }
    Ok(first)
}

/// `<path> (id <id>)`, with `, amd64 <level>` appended on an x86_64 slice:
/// what `builds[].id` and `universal_binaries[].ids` would have to name to
/// pick this slice. An arm64 slice has no amd64 level.
fn describe_slice(a: &Artifact) -> String {
    let id = a.metadata.get("id").map(String::as_str).unwrap_or("");
    let level = a
        .metadata
        .get("amd64_variant")
        .map(String::as_str)
        .or_else(|| a.target.as_deref()?.starts_with("x86_64-").then_some("v1"));
    match level {
        Some(level) => format!("{} (id {id}, amd64 {level})", a.path.display()),
        None => format!("{} (id {id})", a.path.display()),
    }
}

pub(crate) fn build_universal_binary(
    crate_name: &str,
    ub: &UniversalBinaryConfig,
    ctx: &mut Context,
    dry_run: bool,
) -> anyhow::Result<()> {
    let log = ctx.logger("build");
    let filtered = selected_binaries(ctx, crate_name, ub);

    let arm64 = one_slice_per_target(&filtered, "aarch64-apple-darwin", crate_name)?;
    let x86_64 = one_slice_per_target(&filtered, "x86_64-apple-darwin", crate_name)?;

    let (arm64_path, x86_64_path) = match (arm64, x86_64) {
        (Some(a), Some(x)) => (a.path.clone(), x.path.clone()),
        _ => {
            // Not an error: universal binaries require both darwin archs, which
            // only exist on macOS builds or in merge mode. On Linux/Windows split
            // builds this skip is expected and is no strict_guard situation.
            log.verbose(&format!(
                "skipped universal binary for {crate_name} — \
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
    // The default `name_template` is
    // `{{ .ProjectName }}`, NOT the source binary filename. The default is
    // rendered explicitly so `.exe`-suffixed source names and custom
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
    // the convention for universal binaries.
    let dist_dir = &ctx.config.dist;
    let ub_dir = dist_dir.join(format!("{}_darwin_all", crate_name));
    let out_path = ub_dir.join(&out_name);

    // Execute pre-hooks if configured
    let template_vars = ctx.template_vars().clone();
    if let Some(ref hooks) = ub.hooks
        && let Some(ref pre) = hooks.pre
    {
        // Universal-binary hooks are not build hooks (no builds[].env applies);
        // The universal-binary run hook injects only ctx env + hook env.
        run_hooks(
            pre,
            "pre-universal-binary",
            HookRunContext::new(dry_run, &log, Some(&template_vars)),
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
        if !on_path("lipo") {
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
            let stderr = log.redact(String::from_utf8_lossy(&output.stderr).trim());
            anyhow::bail!("lipo failed for {crate_name}: {stderr}");
        }
    }

    // Execute post-hooks if configured
    if let Some(ref hooks) = ub.hooks
        && let Some(ref post) = hooks.post
    {
        run_hooks(
            post,
            "post-universal-binary",
            HookRunContext::new(dry_run, &log, Some(&template_vars)),
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

    // Every metadata key of the first source binary is copied, then the
    // universal-specific keys overwrite: a key whitelist would drop whatever
    // stage-build adds later (`DynamicallyLinked`, `amd64_variant`, …).
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
    // Without a configured id the first source's `id` stays as copied.
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
