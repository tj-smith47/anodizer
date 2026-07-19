use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;

use crate::arch::{is_valid_snap_arch, triple_to_snap_arch};
use crate::command::snapcraft_command;
use crate::gate::{compute_snap_filename, validate_and_check_skip};
use crate::prime::stage_prime_dir;
use crate::yaml::DEFAULT_SNAP_NAME_TEMPLATE;

// snapcraft ≤8.14.5: `snapcraft_legacy.internal.repo._deb` runs
// `BaseDirectory.save_cache_path("snapcraft", "download")` at import time, which
// calls `os.makedirs(path)` without `exist_ok=True`. Once the first invocation
// creates that directory, every subsequent snapcraft process crashes at import
// before it can pack. We wipe the cache dir and serialize invocations so the
// wipe-then-pack sequence is atomic across parallel workers.
static SNAPCRAFT_CACHE_LOCK: Mutex<()> = Mutex::new(());

/// Resolve the snapcraft download-cache directory from XDG/HOME semantics.
///
/// `XDG_CACHE_HOME` wins when set (snapcraft uses `xdg.BaseDirectory`,
/// which honors it); otherwise falls back to `<home>/.cache`. Returns the
/// exact `…/snapcraft/download` leaf — never a parent — so a wipe can
/// only ever touch the directory snapcraft re-creates at import time.
fn snapcraft_download_cache(xdg_cache_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_cache_home.filter(|s| !s.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|s| !s.is_empty())?).join(".cache"),
    };
    Some(base.join("snapcraft").join("download"))
}

/// Wipe snapcraft's download cache to dodge the ≤8.14.5 import-time crash
/// (see `SNAPCRAFT_CACHE_LOCK`). Only removes the `…/snapcraft/download`
/// leaf, only when it already exists, and logs (at verbose) what is being
/// cleared and why so the destructive step is auditable under `-v`.
fn clear_snapcraft_cache(xdg_cache_home: Option<&str>, home: Option<&str>, log: &StageLogger) {
    let Some(cache) = snapcraft_download_cache(xdg_cache_home, home) else {
        return;
    };
    if !cache.exists() {
        return;
    }
    log.verbose(&format!(
        "clearing stale download cache at {} (works around snapcraft ≤8.14.5 import-time os.makedirs crash)",
        cache.display()
    ));
    let _ = std::fs::remove_dir_all(&cache);
}

// ---------------------------------------------------------------------------
// Icon copy helper
// ---------------------------------------------------------------------------

/// Resolve a user-configured icon path against the project root.
///
/// Absolute paths pass through unchanged. Relative paths are joined to
/// `project_root` when set, otherwise to the process CWD (`.`). Used by
/// both the early-validation site and the actual copy site so the two
/// can't drift on resolution rules.
pub(crate) fn resolve_icon_path(icon_src_str: &str, project_root: Option<&PathBuf>) -> PathBuf {
    if std::path::Path::new(icon_src_str).is_absolute() {
        PathBuf::from(icon_src_str)
    } else {
        project_root
            .map(|p| p.as_path())
            .unwrap_or(std::path::Path::new("."))
            .join(icon_src_str)
    }
}

/// Copy a snap icon source file into `<meta_dir>/gui/<snap_name>.<ext>`.
///
/// `icon_src`: the resolved path (absolute when `project_root` is set,
/// otherwise relative to the process CWD).
/// `meta_dir` is the `prime/meta/` directory that snapcraft reads from
/// when packing a pre-staged prime dir. `snap_name` is the snap's name
/// field — the destination filename matches the name so the Store's GUI
/// metadata channel picks up the correct icon.
///
/// Extension is preserved from the source so `.svg` icons round-trip
/// correctly alongside `.png`. The caller is expected to have already
/// validated the extension against snapcraft's allowed set.
pub(crate) fn copy_snap_icon(
    icon_src: &std::path::Path,
    meta_dir: &std::path::Path,
    snap_name: &str,
) -> anyhow::Result<String> {
    let ext = icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let gui_dir = meta_dir.join("gui");
    fs::create_dir_all(&gui_dir)
        .with_context(|| format!("snapcraft: create meta/gui dir: {}", gui_dir.display()))?;
    let dest_name = format!("{}.{}", snap_name, ext);
    let icon_dest = gui_dir.join(&dest_name);
    fs::copy(icon_src, &icon_dest).with_context(|| {
        format!(
            "snapcraft: copy icon {} to {}",
            icon_src.display(),
            icon_dest.display()
        )
    })?;
    Ok(format!("meta/gui/{}", dest_name))
}

// ---------------------------------------------------------------------------
// SnapcraftStage
// ---------------------------------------------------------------------------

pub struct SnapcraftStage;

/// A fully-staged snapcraft job ready for parallel `snapcraft pack`
/// invocation. Step 1 (serial, `&mut ctx`) stages the prime dir and
/// renders templates; Step 2 (parallel) runs the subprocess. `_tmp_dir`
/// keeps the staging dir alive through Step 2 — its `Drop` deletes the
/// directory when the job's worker thread finishes.
struct SnapcraftJob {
    _tmp_dir: tempfile::TempDir,
    snap_path: PathBuf,
    cmd_args: Vec<String>,
    target: Option<String>,
    crate_name: String,
    artifact_metadata: HashMap<String, String>,
}

impl Stage for SnapcraftStage {
    fn name(&self) -> &str {
        "snapcraft"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have snapcraft config
        let crates: Vec<_> = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();
        let mut jobs: Vec<SnapcraftJob> = Vec::new();

        for krate in &crates {
            let Some(snap_configs) = krate.snapcrafts.as_ref() else {
                continue;
            };

            // Collect all Linux binary artifacts for this crate, cloned so the
            // mutable `process_snap_target` borrow below does not conflict with
            // the artifact-registry borrow.
            let linux_binaries: Vec<Artifact> = linux_binaries_for_crate(ctx, &krate.name)
                .into_iter()
                .cloned()
                .collect();

            // One guard per crate spans every `snapcrafts:` config of that crate:
            // two configs with the default (or identical) `name:` render the same
            // `.snap` path for one arch — error loudly across configs instead of
            // letting the second silently clobber the first.
            let mut name_guard = anodizer_core::arch_path_guard::ArchPathGuard::new();

            for snap_cfg in snap_configs {
                if validate_and_check_skip(ctx, &log, snap_cfg, &krate.name)? {
                    continue;
                }

                let linux_refs: Vec<&Artifact> = linux_binaries.iter().collect();
                let filtered_binaries = filter_binaries_by_ids(&linux_refs, snap_cfg.ids.as_ref());

                // Warn and skip if no linux binaries found
                if filtered_binaries.is_empty() && linux_binaries.is_empty() {
                    log.warn(&format!(
                        "skipped snapcraft for crate '{}' — no Linux binaries found",
                        krate.name
                    ));
                    continue;
                }
                if filtered_binaries.is_empty() {
                    log.warn(&format!(
                        "skipped snapcraft for crate '{}' — ids filter {:?} matched no binaries",
                        krate.name, snap_cfg.ids
                    ));
                    continue;
                }

                let by_target = group_binaries_by_target(&filtered_binaries);

                for ((target_key, amd64_variant), target_binaries) in &by_target {
                    process_snap_target(
                        ctx,
                        &log,
                        snap_cfg,
                        &krate.name,
                        target_key,
                        amd64_variant.as_deref(),
                        target_binaries,
                        &dist,
                        &version,
                        dry_run,
                        &mut new_artifacts,
                        &mut archives_to_remove,
                        &mut jobs,
                        &mut name_guard,
                    )?;
                }
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        if !jobs.is_empty() {
            let home_for_cache = ctx.env_var("HOME");
            let xdg_cache_home = ctx.env_var("XDG_CACHE_HOME");
            let packed = run_snap_jobs(
                &jobs,
                xdg_cache_home.as_deref(),
                home_for_cache.as_deref(),
                &log,
                parallelism,
            )?;
            new_artifacts.extend(packed);
        }

        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Group key for one snap: target triple plus the amd64 micro-architecture
/// variant. Two amd64 builds of one triple (baseline `v1` and, e.g., `v3`)
/// share `Os`/`Arch` but must produce distinct snaps, so the variant is part
/// of the grouping key.
type SnapTargetKey = (String, Option<String>);

/// Group a crate's Linux binary artifacts by `(target triple, amd64_variant)`
/// — one snap per platform-variant. `BTreeMap` (not `HashMap`) so iteration
/// order is deterministic across runs; the map is iterated to register one
/// snap Artifact per key, and `HashMap`'s randomised iteration would bake
/// per-run order into `dist/artifacts.json`. A binary with no target lands
/// under the `unknown` key (a host-target build with no triple). Both the
/// build's `run` loop and the offline `snapcraft_snap_yamls_for_crate`
/// renderer call this so the two can never diverge on grouping.
pub(crate) fn group_binaries_by_target<'a>(
    binaries: &[&'a Artifact],
) -> BTreeMap<SnapTargetKey, Vec<&'a Artifact>> {
    let mut by_target: BTreeMap<SnapTargetKey, Vec<&Artifact>> = BTreeMap::new();
    for b in binaries {
        let target = b.target.clone().unwrap_or_else(|| "unknown".to_string());
        let variant = b.metadata.get("amd64_variant").cloned();
        by_target.entry((target, variant)).or_default().push(b);
    }
    by_target
}

/// Collect a crate's Linux binary artifacts in artifact-registry order.
///
/// Both the build loop and the offline renderer start from this exact set
/// before applying the per-config `ids` filter, so the validated universe
/// equals the published universe.
pub(crate) fn linux_binaries_for_crate<'a>(
    ctx: &'a Context,
    crate_name: &str,
) -> Vec<&'a Artifact> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_linux)
                .unwrap_or(false)
        })
        .collect()
}

/// Apply a snap config's `ids` allow-list to a crate's Linux binaries.
///
/// An empty / absent `ids` admits every binary. A non-empty `ids` keeps only
/// binaries whose `id` or `name` metadata matches. Shared by the build loop
/// and the offline renderer so both honour identical eligibility.
pub(crate) fn filter_binaries_by_ids<'a>(
    binaries: &[&'a Artifact],
    ids: Option<&Vec<String>>,
) -> Vec<&'a Artifact> {
    let mut filtered: Vec<&Artifact> = binaries.to_vec();
    if let Some(filter_ids) = ids
        && !filter_ids.is_empty()
    {
        filtered.retain(|b| {
            b.metadata
                .get("id")
                .map(|id| filter_ids.contains(id))
                .unwrap_or(false)
                || b.metadata
                    .get("name")
                    .map(|n| filter_ids.contains(n))
                    .unwrap_or(false)
        });
    }
    filtered
}

// ---------------------------------------------------------------------------
// Per-target helper
// ---------------------------------------------------------------------------

/// Process one target platform for a snapcraft config: validate arch,
/// compute filename, and either emit dry-run output or stage the prime
/// directory and enqueue a `SnapcraftJob`.
#[allow(clippy::too_many_arguments)]
fn process_snap_target(
    ctx: &mut Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    crate_name: &str,
    target_key: &str,
    amd64_variant: Option<&str>,
    target_binaries: &[&Artifact],
    dist: &Path,
    version: &str,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    archives_to_remove: &mut Vec<PathBuf>,
    jobs: &mut Vec<SnapcraftJob>,
    name_guard: &mut anodizer_core::arch_path_guard::ArchPathGuard,
) -> Result<()> {
    let target = if target_key == "unknown" {
        None
    } else {
        Some(target_key.to_string())
    };

    if let Some(ref t) = target {
        let snap_arch = triple_to_snap_arch(t);
        if !is_valid_snap_arch(snap_arch) {
            log.warn(&format!(
                "skipped arch '{}' — unsupported (target: {})",
                snap_arch, t
            ));
            return Ok(());
        }
    }

    let (os, arch) = target
        .as_deref()
        .map(anodizer_core::target::map_target)
        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

    let output_dir = dist.join("linux");
    if !dry_run {
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("create snapcraft output dir: {}", output_dir.display()))?;
    }

    let snap_name = snap_cfg.name.as_deref().unwrap_or(crate_name);
    let snap_filename = compute_snap_filename(
        ctx,
        snap_cfg,
        crate_name,
        snap_name,
        target.as_deref(),
        &os,
        &arch,
        amd64_variant,
    )?;
    let snap_path = output_dir.join(&snap_filename);
    let name_template = snap_cfg
        .name_template
        .as_deref()
        .unwrap_or(DEFAULT_SNAP_NAME_TEMPLATE);
    name_guard.check(
        &snap_path,
        "snapcrafts",
        "snap",
        name_template,
        &snap_filename,
        crate_name,
    )?;

    let artifact_metadata = {
        let mut m = HashMap::new();
        if let Some(id) = &snap_cfg.id {
            m.insert("id".to_string(), id.clone());
        }
        m
    };

    if dry_run {
        log.status(&format!(
            "(dry-run) would run: snapcraft pack --output {} for crate {} target {:?}",
            snap_path.display(),
            crate_name,
            target,
        ));
        new_artifacts.push(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: snap_path,
            target: target.clone(),
            crate_name: crate_name.to_string(),
            metadata: artifact_metadata,
            size: None,
        });
        archives_to_remove.extend(anodizer_core::util::collect_if_replace(
            snap_cfg.replace,
            &ctx.artifacts,
            crate_name,
            target.as_deref(),
        ));
        return Ok(());
    }

    let (tmp_dir, prime_dir) = stage_prime_dir(
        ctx,
        log,
        snap_cfg,
        crate_name,
        snap_name,
        target_binaries,
        target.as_deref(),
        version,
    )?;

    let cmd_args = snapcraft_command(&prime_dir.to_string_lossy(), &snap_path.to_string_lossy());

    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
        snap_cfg.replace,
        &ctx.artifacts,
        crate_name,
        target.as_deref(),
    ));

    jobs.push(SnapcraftJob {
        _tmp_dir: tmp_dir,
        snap_path,
        cmd_args,
        target: target.clone(),
        crate_name: crate_name.to_string(),
        artifact_metadata,
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run all staged `snapcraft pack` jobs with bounded parallelism.
/// Serializes the cache-wipe + pack pair across workers via
/// `SNAPCRAFT_CACHE_LOCK` so each invocation sees a non-existent cache
/// dir at import time.
fn run_snap_jobs(
    jobs: &[SnapcraftJob],
    xdg_cache_home: Option<&str>,
    home_for_cache: Option<&str>,
    log: &StageLogger,
    parallelism: usize,
) -> Result<Vec<Artifact>> {
    let run_job = |job: &SnapcraftJob| -> Result<Artifact> {
        let thread_log = StageLogger::new("snapcraft", log.verbosity());

        let _cache_guard = SNAPCRAFT_CACHE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("snapcraft cache lock poisoned"))?;
        clear_snapcraft_cache(xdg_cache_home, home_for_cache, &thread_log);

        thread_log.verbose(&format!("running {}", job.cmd_args.join(" ")));

        let mut command = Command::new(&job.cmd_args[0]);
        command.args(&job.cmd_args[1..]);
        anodizer_core::run::run_checked(&mut command, &thread_log, "snapcraft pack")?;

        let snap_name = job
            .snap_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.snap_path.display().to_string());
        thread_log.status(&format!("packed snap {snap_name}"));

        Ok(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: job.snap_path.clone(),
            target: job.target.clone(),
            crate_name: job.crate_name.clone(),
            metadata: job.artifact_metadata.clone(),
            size: None,
        })
    };

    anodizer_core::parallel::run_parallel_chunks(jobs, parallelism, "snapcraft", log, run_job)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use anodizer_core::log::Verbosity;

    #[test]
    fn xdg_cache_home_wins_over_home() {
        let resolved = snapcraft_download_cache(Some("/xdg/cache"), Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/xdg/cache/snapcraft/download"))
        );
    }

    #[test]
    fn falls_back_to_home_dot_cache() {
        let resolved = snapcraft_download_cache(None, Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/home/u/.cache/snapcraft/download"))
        );
    }

    #[test]
    fn empty_xdg_falls_back_to_home() {
        let resolved = snapcraft_download_cache(Some(""), Some("/home/u"));
        assert_eq!(
            resolved,
            Some(PathBuf::from("/home/u/.cache/snapcraft/download"))
        );
    }

    #[test]
    fn no_home_and_no_xdg_resolves_nothing() {
        assert_eq!(snapcraft_download_cache(None, None), None);
        assert_eq!(snapcraft_download_cache(Some(""), Some("")), None);
    }

    #[test]
    fn target_is_the_download_leaf_never_a_parent() {
        let resolved = snapcraft_download_cache(None, Some("/home/u")).unwrap();
        assert!(resolved.ends_with("snapcraft/download"));
    }

    #[test]
    fn clear_only_removes_existing_download_subdir_not_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path();
        let download = xdg.join("snapcraft").join("download");
        std::fs::create_dir_all(&download).unwrap();
        // A sibling under the snapcraft cache that must survive the wipe.
        let sibling = xdg.join("snapcraft").join("keepme");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(download.join("blob.bin"), b"stale").unwrap();

        let log = StageLogger::new("snapcraft-cache-test", Verbosity::Quiet);
        clear_snapcraft_cache(Some(xdg.to_str().unwrap()), None, &log);

        assert!(!download.exists(), "download leaf must be wiped");
        assert!(
            sibling.exists(),
            "wipe must not touch the snapcraft cache parent or its siblings"
        );
    }

    #[test]
    fn clear_is_a_noop_when_cache_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("does-not-exist");
        let log = StageLogger::new("snapcraft-cache-test", Verbosity::Quiet);
        // Must not error or create anything.
        clear_snapcraft_cache(Some(xdg.to_str().unwrap()), None, &log);
        assert!(!xdg.join("snapcraft").exists());
    }
}

#[cfg(test)]
mod id_binding_tests {
    use super::*;
    use std::collections::HashMap;

    fn bin(path: &str, target: &str, id: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(path),
            target: Some(target.to_string()),
            crate_name: "anodizer".to_string(),
            metadata: HashMap::from([("id".to_string(), id.to_string())]),
            size: None,
        }
    }

    fn bin_variant(path: &str, target: &str, variant: Option<&str>) -> Artifact {
        let mut metadata = HashMap::new();
        if let Some(v) = variant {
            metadata.insert("amd64_variant".to_string(), v.to_string());
        }
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(path),
            target: Some(target.to_string()),
            crate_name: "anodizer".to_string(),
            metadata,
            size: None,
        }
    }

    #[test]
    fn ids_filter_binds_gnu_build_excludes_musl() {
        // The snap must ship the glibc binary. snapcraft groups by Os/Arch and
        // map_target collapses gnu and musl x86_64 to linux/amd64, so without
        // an `ids:` bind the musl binary renders the same linux_amd64 snap name
        // and clobbers (last-writer-wins). `ids: [anodizer]` must keep ONLY the
        // gnu build, never the musl one.
        let gnu = bin("dist/anodizer-gnu", "x86_64-unknown-linux-gnu", "anodizer");
        let musl = bin(
            "dist/anodizer-musl",
            "x86_64-unknown-linux-musl",
            "anodizer-musl",
        );
        let all = vec![&gnu, &musl];

        let ids = vec!["anodizer".to_string()];
        let filtered = filter_binaries_by_ids(&all, Some(&ids));

        assert_eq!(filtered.len(), 1, "exactly the gnu build survives the bind");
        let t = filtered[0].target.as_deref().unwrap_or("");
        assert!(
            t.contains("-linux-gnu"),
            "bound build must be the gnu binary, got target {t:?}"
        );
        assert!(
            !filtered
                .iter()
                .any(|b| b.target.as_deref().unwrap_or("").contains("-linux-musl")),
            "the musl binary must never reach the glibc snap"
        );
    }

    #[test]
    fn no_ids_auto_collects_both_builds_the_collision_we_guard_against() {
        // Documents the auto-collect hazard the bind fixes: with `ids` unset
        // BOTH x86_64 builds are admitted, and since they share os/arch the
        // snap filename collides. This is exactly why the config sets
        // `ids: [anodizer]` on `snapcrafts:`.
        let gnu = bin("dist/anodizer-gnu", "x86_64-unknown-linux-gnu", "anodizer");
        let musl = bin(
            "dist/anodizer-musl",
            "x86_64-unknown-linux-musl",
            "anodizer-musl",
        );
        let all = vec![&gnu, &musl];

        let filtered = filter_binaries_by_ids(&all, None);
        assert_eq!(filtered.len(), 2, "auto-collect admits both (the hazard)");
    }

    #[test]
    fn grouping_keys_on_target_and_amd64_variant() {
        // Two amd64 builds of one triple (baseline + v3) share Os/Arch but must
        // land in separate groups so each renders its own snap.
        let v1 = bin_variant("dist/anodizer-v1", "x86_64-unknown-linux-gnu", None);
        let v3 = bin_variant("dist/anodizer-v3", "x86_64-unknown-linux-gnu", Some("v3"));
        let all = vec![&v1, &v3];

        let groups = group_binaries_by_target(&all);
        assert_eq!(groups.len(), 2, "two variants form two distinct groups");
        assert!(groups.contains_key(&("x86_64-unknown-linux-gnu".to_string(), None)));
        assert!(groups.contains_key(&(
            "x86_64-unknown-linux-gnu".to_string(),
            Some("v3".to_string())
        )));
    }

    /// The default snap name must equal core's default asset-name stem plus
    /// `.snap` for every target shape (arm split, mips whole-token, amd64
    /// baseline) — snaps carry the same name every sibling artifact derives
    /// for the target, never a privately-translated scheme.
    #[test]
    fn default_snap_name_equals_core_default_stem() {
        use anodizer_core::test_helpers::TestContextBuilder;
        let cases: &[(&str, &str)] = &[
            ("x86_64-unknown-linux-gnu", "mysnap_1.2.3_linux_amd64.snap"),
            ("aarch64-unknown-linux-gnu", "mysnap_1.2.3_linux_arm64.snap"),
            (
                "armv7-unknown-linux-gnueabihf",
                "mysnap_1.2.3_linux_armv7.snap",
            ),
            (
                "mips64el-unknown-linux-gnuabi64",
                "mysnap_1.2.3_linux_mips64el.snap",
            ),
        ];
        for (target, expected) in cases {
            let mut ctx = TestContextBuilder::new()
                .project_name("myapp")
                .tag("v1.2.3")
                .build();
            let (os, arch) = anodizer_core::target::map_target(target);
            let name = compute_snap_filename(
                &mut ctx,
                &SnapcraftConfig::default(),
                "myapp",
                "mysnap",
                Some(target),
                &os,
                &arch,
                None,
            )
            .expect("render snap filename");
            assert_eq!(&name, expected, "snap name for {target}");

            // Cross-check against core's own render of the same default.
            let mut core_ctx = TestContextBuilder::new()
                .project_name("myapp")
                .tag("v1.2.3")
                .build();
            core_ctx.template_vars_mut().set("ProjectName", "mysnap");
            let stem = anodizer_core::archive_name::render_archive_stem(
                &mut core_ctx,
                anodizer_core::archive_name::DEFAULT_NAME_TEMPLATE,
                target,
            )
            .unwrap();
            assert_eq!(
                name,
                format!("{stem}.snap"),
                "core stem parity for {target}"
            );
        }
    }

    #[test]
    fn same_triple_multi_variant_yields_distinct_snap_names() {
        // 3 amd64 variants of one triple + 1 arm64 → 4 distinct .snap filenames
        // under the default template (no clobber).
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("anodizer")
            .tag("v1.2.3")
            .build();

        let snap_cfg = SnapcraftConfig::default();
        let cases: [(&str, &str, Option<&str>); 4] = [
            ("amd64", "x86_64-unknown-linux-gnu", None),
            ("amd64", "x86_64-unknown-linux-gnu", Some("v2")),
            ("amd64", "x86_64-unknown-linux-gnu", Some("v3")),
            ("arm64", "aarch64-unknown-linux-gnu", None),
        ];

        let mut names = std::collections::HashSet::new();
        for (arch, target, variant) in cases {
            let name = compute_snap_filename(
                &mut ctx,
                &snap_cfg,
                "anodizer",
                "anodizer",
                Some(target),
                "linux",
                arch,
                variant,
            )
            .expect("render snap filename");
            assert!(names.insert(name.clone()), "duplicate snap name: {name}");
        }
        assert_eq!(
            names.len(),
            4,
            "all four targets/variants produce distinct names"
        );
    }

    #[test]
    fn host_build_resets_every_variant_var() {
        // A host-target (no triple) render after an aarch64 target render:
        // the stale `Arm64="v8"` (and any other variant var) must not leak
        // into a user name_template that references it.
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.2.3")
            .build();

        // Prior target render seeds Arm64="v8" (plus I386 via a 386 pass).
        ctx.template_vars_mut().set("Arm64", "v8");
        ctx.template_vars_mut().set("I386", "sse2");
        ctx.template_vars_mut().set("Arm", "7");
        ctx.template_vars_mut().set("Mips", "stale");

        let snap_cfg = SnapcraftConfig {
            name_template: Some(
                "{{ ProjectName }}_{{ Os }}_{{ Arch }}{{ Arm }}{{ Arm64 }}{{ Amd64 }}{{ Mips }}{{ I386 }}"
                    .to_string(),
            ),
            ..Default::default()
        };
        let name = compute_snap_filename(
            &mut ctx, &snap_cfg, "myapp", "mysnap", None, "linux", "amd64", None,
        )
        .expect("render snap filename");
        // Amd64 renders the unified untagged baseline; every other variant
        // var must be empty — no `v8`/`sse2`/`7`/`stale` leak.
        assert_eq!(name, "mysnap_linux_amd64v1.snap");
    }
}
