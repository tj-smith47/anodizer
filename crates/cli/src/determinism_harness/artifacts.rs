//! Per-run artifact discovery, hashing, and drift-bin dump/prune.
//!
//! - [`discover_artifacts`] walks `<worktree>/dist` and surfaces the
//!   shipped raw cargo binaries under `<worktree>/.det-tmp/target/`. The
//!   per-triple `<triple>/release/<bin>` outputs are always surfaced; the
//!   bare host `release/<bin>` is surfaced only when no per-triple build
//!   is present (a host-native consumer with no `--target`). When any
//!   `<triple>/release/` exists the bare host binary is a non-shipped
//!   tooling byproduct (the man-page `before:` hook's `cargo run` output)
//!   and is excluded from the comparison set.
//! - [`hash_artifacts`] SHA256s every artifact and returns
//!   `{name -> ArtifactInfo}` (hash + size + path + stage
//!   attribution + head/tail samples).
//! - [`copy_artifacts_to_dump`] / [`prune_dump_to_drifted`] dump the
//!   per-run binaries to `<report_parent>/drift-bins/run-<N>/` and
//!   then keep only the drifted ones so the artifact upload stays
//!   compact while preserving the diagnostic escape hatch.
//!
//! Preserve-dist (the `--preserve-dist=<path>` flag's copy + context.json
//! emission) is a separate concern with a different lifecycle — see
//! the sibling [`super::preserve`] module.

use anodizer_core::DeterminismReport;
use anodizer_core::log::StageLogger;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-run artifact info captured by [`hash_artifacts`]. Internal to
/// the parent module; `pub(super)` so `Harness::build_report` can read
/// `hash` / `size_bytes` / `relative_path` / `stage` and
/// [`super::drift::summarize_drift`] can read the head/tail samples.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactInfo {
    pub(super) hash: String,
    pub(super) size_bytes: u64,
    /// Path relative to the worktree root (with leading `dist/` etc).
    /// Used as the canonical `ArtifactRow.path` value.
    pub(super) relative_path: String,
    /// Best-effort stage attribution from the path prefix.
    pub(super) stage: String,
    /// First [`HEAD_SAMPLE_BYTES`] bytes of the artifact, retained so
    /// the harness can populate `DriftRow.differing_bytes_summary`
    /// after the worktree is dropped. Why a head sample (not the full
    /// content): the largest artifact in the pipeline is the raw
    /// `.exe` at ~50 MB; multiplied by N runs and ~50 artifacts/run
    /// the retained bytes would blow past the report file's useful
    /// size. The head is what matters for PE / archive / Mach-O drift
    /// (their metadata is front-loaded), and the sample is read
    /// once during the existing `std::fs::read` so there's no extra
    /// I/O.
    pub(super) head_sample: Vec<u8>,
    /// Last [`TAIL_SAMPLE_BYTES`] bytes of the artifact. Complements
    /// `head_sample`: trailing structures that drift past 1 KiB —
    /// gzip footer (`mtime`, ISIZE), zstd skippable frames, ZIP
    /// central directory, PE Debug Directory contents, detached
    /// signature `.sig` trailers — get a localized offset instead of
    /// `"no diff in first 1 KiB"`. Empty when the artifact is smaller
    /// than the head window (the head already covers the whole file).
    pub(super) tail_sample: Vec<u8>,
    /// Complete bytes, retained ONLY for *aggregate* artifacts (combined
    /// checksums, `artifacts.json`) per
    /// [`anodizer_core::determinism::aggregate_kind_for`]. Aggregates are
    /// small (KB) and the transitive-derivation rule needs their full
    /// content to reconstruct members from both runs — head/tail sampling
    /// would force a false fail-closed on any aggregate larger than the
    /// sampled window. `None` for every non-aggregate artifact (the 50 MB
    /// binaries never retain their full bytes).
    pub(super) full: Option<Vec<u8>>,
}

/// How many leading bytes of each artifact to retain for drift
/// diagnostics. 16 KiB covers:
///   - PE: DOS stub + PE signature + COFF header + Optional header +
///     several pages of the .text section. Catches `TimeDateStamp`,
///     `MajorLinkerVersion`, debug directory RVA, and the Rich header.
///   - tar.gz: gzip header + first tar entry header + early file
///     bodies. Catches gzip `mtime` and tar `mtime` drift.
///   - zip: local file header + filename + first file's data start.
///   - CycloneDX SBOM JSON: top-level keys including
///     `serialNumber` (per-run UUID — a known drift source).
pub(super) const HEAD_SAMPLE_BYTES: usize = 16 * 1024;

/// How many trailing bytes of each artifact to retain alongside the
/// head sample. Catches trailing-section drift that the head misses:
///   - gzip footer: 4-byte `mtime` + 4-byte ISIZE.
///   - zstd: skippable frames + content checksum (last 4 B).
///   - ZIP: central directory record + end-of-central-directory
///     record (`EOCD`) including the per-archive comment.
///   - PE: Debug Directory contents (GUID + age + PDB path), import
///     address table, resource section drift.
///   - Detached signatures (`.sig`): cosign/gpg signature blob lives
///     entirely past the head window.
pub(super) const TAIL_SAMPLE_BYTES: usize = 16 * 1024;

/// Walk `<worktree>/dist` and collect every regular file. Sorted by path
/// for deterministic iteration order in tests.
///
/// Also surfaces the **shipped raw cargo build outputs** under
/// `<worktree>/.det-tmp/target/`. These are the SOURCE of any RUSTFLAGS /
/// mtime / build-script drift that later propagates into every wrapped
/// archive (`.tar.gz`, `.tar.xz`, `.zip`, ...). Hashing them directly
/// lets the report point a finger at the raw binary instead of the
/// operator having to peel six layers of containers to find that the
/// underlying `target/release/anodize` was nondeterministic.
/// Path-remapping (`--remap-path-prefix`) is already applied via the env
/// block, so on a healthy run these hashes will match; if they ever
/// drift, the diagnostic chain starts here.
///
/// Which binaries count as shipped depends on the build layout:
///   - `<cargo_target>/<triple>/release/<bin>` — always surfaced; these
///     are the pre-package binaries that land in archives.
///   - `<cargo_target>/release/<bin>` (bare host, no triple) — surfaced
///     ONLY when no `<triple>/release/` directory exists (a host-native
///     consumer building with no `--target`). When any per-triple build
///     is present, the bare host binary is a non-shipped tooling
///     byproduct — e.g. the man-page `before:` hook runs
///     `cargo run --release --bin anodizer -- man` to emit `dist/anodizer.1`,
///     leaving a throwaway host binary that never ships and that has hit
///     intermittent codegen non-determinism on windows-msvc — so it is
///     excluded from the comparison set.
///
/// The function only walks the immediate `release/` directory (not
/// `deps/`, `build/`, `.fingerprint/`, etc.) and filters to files
/// without an extension or with `.exe` — anodize ships single-binary
/// crates, so this surfaces the actual `anodize` / `anodize.exe`
/// without dragging in cargo's incremental-build scratch.
pub(super) fn discover_artifacts(worktree_path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let dist = worktree_path.join("dist");
    if dist.exists() {
        visit_dir(&dist, &mut out)?;
    }

    let target_root = worktree_path.join(".det-tmp").join("target");
    if target_root.exists() {
        collect_raw_binaries(&target_root, &mut out)?;
    }

    out.sort();
    Ok(out)
}

fn visit_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            visit_dir(&entry.path(), out)?;
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Collect raw cargo release binaries from `<cargo_target>/[<triple>/]release/`.
///
/// Two layouts, with different shipping semantics:
///
/// - `<cargo_target>/<triple>/release/<bin>` — cross-target build.
///   ALWAYS collected: these are the pre-package shipped binaries.
/// - `<cargo_target>/release/<bin>` — bare host build, no `--target`.
///   Collected ONLY when no `<triple>/release/` directory is present (a
///   host-native consumer building with no `--target`). The exclusion is
///   keyed on the presence of any per-triple build, not on inspecting the
///   binary's provenance. It is sound because anodizer's build stage always
///   pins `--target`, so a shipped binary never lands at the bare
///   `release/<bin>` path — when a per-triple build exists, anything at that
///   path is a host `cargo run` hook byproduct (e.g. the man-page `before:`
///   hook running `cargo run --release --bin anodizer -- man` to emit
///   `dist/anodizer.1`), which has hit intermittent windows-msvc codegen
///   non-determinism. Gating the release on a throwaway hook binary is a
///   false positive, so it is excluded.
///
/// Only the top-level files inside each `release/` directory are emitted.
/// `release/deps`, `release/build`, `release/.fingerprint`, etc. are
/// cargo's internal scratch and not fingerprinted for drift detection.
///
/// File filter: regular files whose extension is empty (`anodize`) or
/// `.exe` (`anodize.exe`). Excludes `.d` (depfiles), `.pdb` (debug
/// symbols), `.rlib`, etc. — those are tooling byproducts, not the
/// shippable binary that lands in archives.
fn collect_raw_binaries(target_root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(target_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target_root.display())),
    };
    // First pass: collect every per-triple `<triple>/release/` binary and
    // note whether any per-triple build is present. The bare host
    // `release/` directory's shipping status depends on that answer, so it
    // is deferred to a second decision below.
    let mut host_release: Option<PathBuf> = None;
    let mut any_triple_release = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name_s == "release" {
            host_release = Some(entry.path());
        } else if name_s == "debug"
            || name_s == ".rustc_info.json"
            || name_s == "CACHEDIR.TAG"
            || name_s.starts_with('.')
        {
            continue;
        } else {
            let release_dir = entry.path().join("release");
            if release_dir.is_dir() {
                any_triple_release = true;
                push_release_dir_files(&release_dir, out)?;
            }
        }
    }
    // The decision keys on whether any per-triple build is present, not on
    // the bare binary's provenance. That is sound because anodizer's build
    // stage always pins `--target`, so a shipped binary never lands at the
    // bare `release/<bin>` path — when a per-triple build exists, anything
    // there is only ever a host `cargo run` hook byproduct (the man-page
    // emitter), never a shipped artifact, so it is excluded.
    if let Some(host_release) = host_release {
        if !any_triple_release {
            push_release_dir_files(&host_release, out)?;
        }
    }
    Ok(())
}

fn push_release_dir_files(release_dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(release_dir)
        .with_context(|| format!("reading {}", release_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        // Skip cargo's `.cargo-*-lock` dotfiles: a leading-dot name has no
        // extension, so the empty-extension binary filter below would mistake
        // them for the shipped binary. A shipped binary is never a dotfile.
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        match path.extension().and_then(|s| s.to_str()) {
            None => out.push(path),
            Some("exe") => out.push(path),
            _ => continue,
        }
    }
    Ok(())
}

/// SHA256 every artifact and return `{name -> info}`.
///
/// Map keys are relative paths stripped of the leading `dist/` prefix
/// and forward-slash-normalized. For `dist/` files this is the path
/// under the dist root (e.g. `makeself/default/linux_amd64/anodizer`),
/// which avoids basename collisions when multiple arch directories
/// contain a file with the same name. Raw cargo binaries under
/// `<worktree>/.det-tmp/target` get a `target/<triple>/<bin>` key so
/// the report unambiguously distinguishes them from same-basename
/// `dist/` artifacts.
pub(super) fn hash_artifacts(
    worktree_path: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<String, ArtifactInfo>> {
    use sha2::{Digest, Sha256};
    let mut out = BTreeMap::new();
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let bytes =
            std::fs::read(p).with_context(|| format!("reading artifact {}", p.display()))?;
        // Bytes are already in memory (head/tail samples + size below need
        // them), so hash the one-shot buffer rather than re-opening the
        // file via `core::hashing::sha256_file`. Format through the shared
        // `hex_lower` helper so the hex encoding stays in one place.
        let digest = format!(
            "sha256:{}",
            anodizer_core::hashing::hex_lower(&Sha256::digest(&bytes))
        );
        let relative = p
            .strip_prefix(worktree_path)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned();
        let name = if let Ok(under_target) = p.strip_prefix(&target_root) {
            // Raw cargo binary: prefix with `target/` and the
            // <triple>/release/ (or release/) segments so the report
            // surfaces it distinctly from any `dist/` artifact of the
            // same basename. Forward slashes regardless of platform
            // (matches `Artifact::to_artifacts_json` normalization).
            let suffix = under_target.to_string_lossy().replace('\\', "/");
            format!("target/{}", suffix)
        } else {
            // Dist artifact: key by path relative to the dist root
            // (forward-slash-normalized, `dist/` prefix stripped).
            // Keying by basename would collapse same-basename files
            // under different arch subdirectories (e.g.
            // `dist/makeself/<id>/linux_amd64/anodizer` and
            // `dist/makeself/<id>/linux_arm64/anodizer` both have
            // basename `anodizer`).
            let dist_root = worktree_path.join("dist");
            if let Ok(under_dist) = p.strip_prefix(&dist_root) {
                under_dist.to_string_lossy().replace('\\', "/")
            } else {
                // Path outside dist/ and outside target/: fall back to
                // relative-from-worktree so the key is still unique.
                p.strip_prefix(worktree_path)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            }
        };
        let stage = infer_stage_from_path(&relative);
        let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
        let head_sample = bytes[..head_len].to_vec();
        // Tail sample is chosen so head + tail together cover every byte
        // of files up to HEAD + TAIL with no unsampled gap:
        //   * len ≤ HEAD            → tail empty (head already covers all)
        //   * HEAD < len ≤ HEAD+TAIL → tail = bytes[HEAD..end] (closes the
        //                              gap; smaller than TAIL but enough
        //                              to keep drift detection contiguous)
        //   * len > HEAD + TAIL     → tail = trailing TAIL_SAMPLE_BYTES
        //                              window (the gap in (HEAD, len-TAIL)
        //                              is genuinely unsampled — too large
        //                              to retain).
        // The earlier shape ("empty when ≤ HEAD+TAIL") created a black
        // hole exactly where mid-size artifacts (artifacts.json at ~24
        // KiB) actually drift — drift detector then couldn't localize.
        let tail_sample = if bytes.len() <= HEAD_SAMPLE_BYTES {
            Vec::new()
        } else {
            let tail_start = bytes
                .len()
                .saturating_sub(TAIL_SAMPLE_BYTES)
                .max(HEAD_SAMPLE_BYTES);
            bytes[tail_start..].to_vec()
        };
        // Retain full bytes for aggregates (so the transitive-derivation rule
        // can reconstruct their members) plus any small text file — the latter
        // covers an operator-renamed combined file (`SHA512SUMS`) whose
        // aggregate identity is only knowable after the manifest is parsed.
        let full = if should_capture_full(&name, &bytes) {
            Some(bytes.clone())
        } else {
            None
        };
        out.insert(
            name,
            ArtifactInfo {
                hash: digest,
                size_bytes: bytes.len() as u64,
                relative_path: relative,
                stage,
                head_sample,
                tail_sample,
                full,
            },
        );
    }
    Ok(out)
}

/// Copy each artifact in `paths` to `dump_root/<artifact-name>`,
/// preserving the relative directory structure under `worktree_path`.
///
/// Best-effort: copy failures are logged but not surfaced, so the
/// harness's primary determinism check is never broken by a side
/// channel diagnostic.
pub(super) fn copy_artifacts_to_dump(
    worktree_path: &Path,
    paths: &[PathBuf],
    dump_root: &Path,
    log: &StageLogger,
) -> Result<()> {
    let target_root = worktree_path.join(".det-tmp").join("target");
    for p in paths {
        let dest_rel = if let Ok(under_target) = p.strip_prefix(&target_root) {
            PathBuf::from("target").join(under_target)
        } else if let Ok(under_worktree) = p.strip_prefix(worktree_path) {
            under_worktree.to_path_buf()
        } else {
            PathBuf::from(p.file_name().unwrap_or_default())
        };
        let dest = dump_root.join(dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dump parent {}", parent.display()))?;
        }
        if let Err(e) = std::fs::copy(p, &dest) {
            log.warn(&format!(
                "drift-bin dump failed for {} → {}: {}",
                p.display(),
                dest.display(),
                e
            ));
        }
    }
    Ok(())
}

/// Prune `<dump_root>/run-<N>/<artifact>` entries whose artifact name
/// does NOT appear in `report.drift`. Keeps the artifact upload
/// compact (drifted binaries only) without sacrificing the per-run
/// dump that the harness captured pre-comparison.
pub(super) fn prune_dump_to_drifted(dump_root: &Path, report: &DeterminismReport) {
    if !dump_root.exists() {
        return;
    }
    // Diagnostic escape hatch: `ANODIZE_KEEP_DUMP=1` skips pruning so the
    // full per-run binary set survives for byte-level diffing. Useful
    // when chasing residual allowlisted non-determinism that the report
    // hides under drift_count=0. Off by default to keep the artifact
    // upload compact in normal CI runs.
    if std::env::var_os("ANODIZE_KEEP_DUMP").is_some() {
        return;
    }
    let drift_names: std::collections::HashSet<&str> =
        report.drift.iter().map(|d| d.artifact.as_str()).collect();
    let Ok(run_dirs) = std::fs::read_dir(dump_root) else {
        return;
    };
    for run_entry in run_dirs.flatten() {
        let run_path = run_entry.path();
        if !run_path.is_dir() {
            continue;
        }
        prune_dump_subtree(&run_path, &run_path, &drift_names);
    }
}

fn prune_dump_subtree(root: &Path, dir: &Path, drift_names: &std::collections::HashSet<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_dump_subtree(root, &path, drift_names);
            if std::fs::read_dir(&path)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&path);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            // `DriftRow.artifact` is the harness map key:
            //   * `dist/*` artifacts → basename (e.g. `"artifacts.json"`)
            //   * raw cargo binaries → `target/<triple>/release/<bin>`
            // The dumped relative path always carries the `dist/<name>` or
            // `target/...` prefix, so a basename-only drift entry would
            // never match the full rel path. Keep the file if EITHER form
            // matches — otherwise legitimate drift bins get silently
            // deleted before the CI upload step ever sees them.
            let basename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !drift_names.contains(rel.as_str()) && !drift_names.contains(basename) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Best-effort stage attribution from the artifact path. The harness
/// does not have access to the pipeline's per-stage Artifact records (it
/// shells to a child process), so it infers from filename extension and
/// path conventions. Falls back to `"unknown"` when nothing matches.
pub(super) fn infer_stage_from_path(rel: &str) -> String {
    let lower = rel.replace('\\', "/").to_lowercase();
    // Raw cargo build output under `<worktree>/.det-tmp/target/...` —
    // attribute to `build` so the report makes the source-of-drift
    // chain explicit (build → archive → checksum → sign).
    if lower.contains("/.det-tmp/target/") || lower.starts_with(".det-tmp/target/") {
        return "build".into();
    }
    // Path-prefix wins over extension matching: the OCI tarball under
    // `dist/docker/` ends in `.tar` and would otherwise misattribute to
    // `archive`. Companion `image.digest` lands here too so both
    // byte-stability inputs group under the same stage row.
    if lower.starts_with("dist/docker/") || lower.contains("/dist/docker/") {
        return "docker".into();
    }
    // Installer-family path prefixes: each stage emits artifacts under
    // a stage-named subdirectory of `dist/`. Path-prefix matching
    // beats extension matching because `.tar` / `.zip` / `.exe` are
    // ambiguous between the archive stage and several installer
    // stages (nsis emits a `.exe` installer; makeself emits a `.run`
    // that may also surface as `.sh`). Anchor on the directory the
    // stage owns and the attribution becomes unambiguous.
    for (prefix, stage) in [
        ("dist/nfpm/", "nfpm"),
        ("dist/msi/", "msi"),
        ("dist/nsis/", "nsis"),
        ("dist/dmg/", "dmg"),
        ("dist/pkg/", "pkg"),
        ("dist/srpm/", "srpm"),
        ("dist/makeself/", "makeself"),
        ("dist/snapcraft/", "snapcraft"),
    ] {
        if lower.starts_with(prefix) || lower.contains(&format!("/{}", prefix)) {
            return stage.into();
        }
    }
    if lower.ends_with(".sig") || lower.ends_with(".pem") || lower.ends_with(".cert") {
        "sign".into()
    } else if lower.contains("checksums")
        || lower.ends_with("sha256sum")
        || lower.ends_with("sha256sums")
        || lower.ends_with(".sha256")
    {
        "checksum".into()
    } else if lower.ends_with(".sbom.json")
        || lower.ends_with(".cdx.json")
        || lower.ends_with(".spdx.json")
    {
        "sbom".into()
    } else if lower.ends_with(".src.rpm") {
        // Source RPM produced by `stage-srpm` — guard before the
        // generic `.rpm` rule below so binary RPM detection doesn't
        // swallow it. `stage-srpm` should already land under the
        // `dist/srpm/` prefix above; this is the trailing-fallback
        // path for builds that emit a `.src.rpm` outside the
        // canonical directory layout.
        "srpm".into()
    } else if lower.ends_with(".rpm") || lower.ends_with(".deb") || lower.ends_with(".apk") {
        // Binary RPM / DEB / APK packages come from `stage-nfpm`.
        // Surfaced via the `dist/nfpm/` prefix above for canonical
        // layouts; this branch catches paths that bypass that prefix.
        "nfpm".into()
    } else if lower.ends_with("-setup.exe") || lower.ends_with("_setup.exe") {
        // Only the `setup`-suffixed `.exe` is the NSIS installer; a bare
        // `*.exe` is the raw build binary, so it must not be swept into an
        // installer class.
        "nsis".into()
    } else if lower.ends_with(".msi") {
        "msi".into()
    } else if lower.ends_with(".dmg") {
        "dmg".into()
    } else if lower.ends_with(".pkg") || lower.ends_with(".mpkg") {
        "pkg".into()
    } else if lower.ends_with(".run") {
        // makeself emits self-extracting shell archives with a `.run`
        // suffix by convention. Anchored on the suffix because the
        // file is mode-0755 plain shell, no magic-byte tell.
        "makeself".into()
    } else if lower.ends_with(".tar.gz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".tar.zst")
        || lower.ends_with(".zip")
        || lower.ends_with(".tar")
    {
        "archive".into()
    } else if lower.ends_with(".crate") {
        "cargo-package".into()
    } else if lower.contains(".app/") {
        // Contents of a macOS `.app` bundle directory (Info.plist, the
        // copied binary, Resources). Pure file assembly at a fixed mtime,
        // produced by stage-appbundle. Attribute the whole subtree so its
        // files classify as a tracked primary on the macOS shard rather
        // than falling through to `unknown`.
        "appbundle".into()
    } else if is_man_page(&lower) {
        // A man page (`name.<1-9>` or `name.<1-9>.gz`) emitted by a
        // `before:` hook (e.g. `cargo run -- man > dist/anodizer.1`).
        // Byte-stable at a fixed SOURCE_DATE_EPOCH; classify it so it is a
        // tracked primary, not an unclassified loose file.
        "man".into()
    } else {
        "unknown".into()
    }
}

/// Upper bound on the bytes retained for a non-aggregate text file. Generous
/// versus any combined checksums / `artifacts.json` (KB-scale even for large
/// workspaces) yet small enough that retaining a stray text artifact can't
/// balloon harness memory.
pub(super) const AGGREGATE_FULL_CAP_BYTES: usize = 512 * 1024;

/// Whether to retain `bytes` in [`ArtifactInfo::full`] for member
/// reconstruction. Filename-recognized aggregates are always retained; any
/// other small text file is retained too, so a marker-renamed combined file
/// (`SHA512SUMS`) — unrecognizable by suffix until the manifest is parsed —
/// still carries bytes the transitive-derivation rule can reconstruct.
pub(super) fn should_capture_full(name: &str, bytes: &[u8]) -> bool {
    if anodizer_core::determinism::aggregate_kind_for(name).is_some() {
        return true;
    }
    bytes.len() <= AGGREGATE_FULL_CAP_BYTES && looks_like_text(bytes)
}

/// Heuristic text test: a NUL byte in the first 8 KiB marks the file binary.
/// Combined checksums and `artifacts.json` are NUL-free UTF-8; archives /
/// packages / binaries are not.
fn looks_like_text(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8192)];
    !probe.contains(&0)
}

/// `true` when the basename of `lower` looks like a Unix man page: a
/// `name.<section>` where `<section>` is a single digit `1`–`9`, with an
/// optional `.gz` compression suffix (`anodizer.1`, `foo.8.gz`). Conservative
/// — false negatives only cost a man page its `man` attribution; false
/// positives would mislabel a real artifact, so the single-digit section is
/// required (a `v1.2.3`-style tail never matches).
fn is_man_page(lower: &str) -> bool {
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(lower);
    // A versioned shared object (`libfoo.so.1`, `libssl.so.1.1`) has the same
    // `name.<single-digit>` tail as a man page but is NOT one — exclude it so
    // its `man` mis-attribution can't hide a real `.so` regression.
    if base.contains(".so.") {
        return false;
    }
    let stem = base.strip_suffix(".gz").unwrap_or(base);
    match stem.rsplit_once('.') {
        Some((name, section)) => {
            !name.is_empty()
                && section.len() == 1
                && section
                    .chars()
                    .next()
                    .is_some_and(|c| ('1'..='9').contains(&c))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_inference_matches_known_extensions() {
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.zip"), "archive");
        assert_eq!(infer_stage_from_path("dist/foo.crate"), "cargo-package");
        assert_eq!(infer_stage_from_path("dist/docker/image.oci.tar"), "docker");
        assert_eq!(infer_stage_from_path("dist/docker/image.digest"), "docker");
        assert_eq!(infer_stage_from_path("dist/foo.sbom.json"), "sbom");
        assert_eq!(infer_stage_from_path("dist/foo.tar.gz.sig"), "sign");
        assert_eq!(infer_stage_from_path("dist/checksums.txt"), "checksum");
        assert_eq!(infer_stage_from_path("dist/SHA256SUMS"), "checksum");
        assert_eq!(infer_stage_from_path("dist/mystery.bin"), "unknown");
        // Windows-native separators must still classify correctly.
        assert_eq!(
            infer_stage_from_path(".det-tmp\\target\\x86_64-pc-windows-msvc\\release\\anodize.exe"),
            "build"
        );
        assert_eq!(infer_stage_from_path("dist\\foo.tar.gz"), "archive");
    }

    /// Installer-family stages emit artifacts under their stage-named
    /// `dist/<stage>/` subdirectory; the `infer_stage_from_path`
    /// classifier must pick those up so the report's per-stage drift
    /// counts attribute correctly. Without this, e.g. an MSI installer
    /// at `dist/msi/anodize-0.4.0.msi` would have shown up under
    /// `unknown` and the report's `drift` row would not have named
    /// the responsible stage.
    /// Man pages from a `before:` hook (`dist/anodizer.1`) and the contents
    /// of a macOS `.app` bundle must classify to a known stage so the
    /// exhaustive classifier does not flag them as unregistered files.
    #[test]
    fn stage_inference_classifies_man_pages_and_app_bundles() {
        assert_eq!(infer_stage_from_path("dist/anodizer.1"), "man");
        assert_eq!(infer_stage_from_path("dist/anodizer.8.gz"), "man");
        assert_eq!(
            infer_stage_from_path("dist/macos/anodizer.app/Contents/MacOS/anodizer"),
            "appbundle"
        );
        assert_eq!(
            infer_stage_from_path("dist/macos/anodizer.app/Contents/Info.plist"),
            "appbundle"
        );
        // A version-suffixed binary must NOT be mistaken for a man page —
        // the section must be a single digit.
        assert_eq!(
            infer_stage_from_path("dist/anodizer-0.12.3-linux-amd64"),
            "unknown"
        );
        assert_eq!(infer_stage_from_path("dist/install.sh"), "unknown");
        // A versioned shared object shares the `name.<single-digit>` tail of a
        // man page but must NOT be classified `man` (else a `.so.N` drift would
        // hide behind a man-page attribution).
        assert_eq!(infer_stage_from_path("dist/lib/libfoo.so.1"), "unknown");
        assert_eq!(infer_stage_from_path("dist/lib/libssl.so.1.1"), "unknown");
        assert_eq!(infer_stage_from_path("dist/lib/libc.so.6"), "unknown");
        // A bare `.so` (no section) was never a man page and stays unknown.
        assert_eq!(infer_stage_from_path("dist/lib/libfoo.so"), "unknown");
    }

    #[test]
    fn stage_inference_classifies_installer_directory_prefixes() {
        assert_eq!(
            infer_stage_from_path("dist/nfpm/anodize_0.4.0_amd64.deb"),
            "nfpm"
        );
        assert_eq!(
            infer_stage_from_path("dist/nfpm/anodize-0.4.0-1.x86_64.rpm"),
            "nfpm"
        );
        assert_eq!(infer_stage_from_path("dist/msi/anodize-0.4.0.msi"), "msi");
        assert_eq!(
            infer_stage_from_path("dist/nsis/anodize-setup-0.4.0.exe"),
            "nsis"
        );
        assert_eq!(infer_stage_from_path("dist/dmg/anodize-0.4.0.dmg"), "dmg");
        assert_eq!(infer_stage_from_path("dist/pkg/anodize-0.4.0.pkg"), "pkg");
        assert_eq!(
            infer_stage_from_path("dist/srpm/anodize-0.4.0-1.src.rpm"),
            "srpm"
        );
        assert_eq!(
            infer_stage_from_path("dist/makeself/anodize-0.4.0.run"),
            "makeself"
        );
        assert_eq!(
            infer_stage_from_path("dist/snapcraft/anodize_0.4.0_amd64.snap"),
            "snapcraft"
        );
    }

    /// Installer artifacts that escape the canonical `dist/<stage>/`
    /// layout (e.g. operator-overridden output paths) must still
    /// attribute to their stage by file extension. Guards the
    /// trailing-fallback branch of `infer_stage_from_path`.
    #[test]
    fn stage_inference_classifies_installer_extensions_outside_prefix() {
        assert_eq!(infer_stage_from_path("dist/anodize-0.4.0.msi"), "msi");
        assert_eq!(infer_stage_from_path("dist/anodize-0.4.0.dmg"), "dmg");
        assert_eq!(infer_stage_from_path("dist/anodize-0.4.0.pkg"), "pkg");
        assert_eq!(infer_stage_from_path("dist/anodize-0.4.0.run"), "makeself");
        assert_eq!(
            infer_stage_from_path("dist/anodize-0.4.0-1.src.rpm"),
            "srpm"
        );
        assert_eq!(
            infer_stage_from_path("dist/anodize-0.4.0-1.x86_64.rpm"),
            "nfpm"
        );
        assert_eq!(
            infer_stage_from_path("dist/anodize_0.4.0_amd64.deb"),
            "nfpm"
        );
        assert_eq!(infer_stage_from_path("dist/anodize-0.4.0.apk"), "nfpm");
    }

    /// The loose-`.exe` collision: stage-nsis writes its installer into
    /// `dist/windows/` as a plain `*.exe` (no `dist/nsis/` prefix), and the
    /// raw windows binary `anodize.exe` lives under the cargo target dir. Both
    /// are `.exe`, so the classifier must tell them apart by the installer's
    /// `setup.exe` name tail — never sweep a raw binary into an installer
    /// class, never leave the installer in `unknown` (which would drop it from
    /// the per-stage drift attribution and re-hide the nsis format).
    #[test]
    fn stage_inference_distinguishes_nsis_installer_from_raw_windows_binary() {
        // The repo config renders `{{ .ProjectName }}_{{ .Arch }}-setup`, and
        // the stage default tails `_setup`; both spellings classify as nsis.
        assert_eq!(
            infer_stage_from_path("dist/windows/anodizer_x64-setup.exe"),
            "nsis"
        );
        assert_eq!(
            infer_stage_from_path("dist/windows/anodizer_x64_setup.exe"),
            "nsis"
        );
        assert_eq!(
            infer_stage_from_path("dist\\windows\\anodizer_arm64-setup.exe"),
            "nsis"
        );
        // The raw windows binary must NOT be misclassified as an installer.
        // Under the cargo target dir it attributes to `build`; a bare loose
        // `anodize.exe` with no installer tail is `unknown`, never `nsis`.
        assert_eq!(
            infer_stage_from_path(".det-tmp/target/x86_64-pc-windows-msvc/release/anodizer.exe"),
            "build"
        );
        assert_eq!(
            infer_stage_from_path("dist/windows/anodizer.exe"),
            "unknown"
        );
        // The MSI sibling in the same dir keeps its own classification.
        assert_eq!(
            infer_stage_from_path("dist/windows/anodizer_x64.msi"),
            "msi"
        );
    }

    /// `.src.rpm` must beat the generic `.rpm` rule — they're
    /// produced by different stages (`stage-srpm` vs `stage-nfpm`)
    /// and a misclassification would make root-causing drift in
    /// either stage harder.
    #[test]
    fn stage_inference_distinguishes_src_rpm_from_binary_rpm() {
        assert_eq!(
            infer_stage_from_path("dist/anodize-0.4.0-1.src.rpm"),
            "srpm"
        );
        assert_eq!(
            infer_stage_from_path("dist/anodize-0.4.0-1.x86_64.rpm"),
            "nfpm"
        );
    }

    /// `discover_artifacts` MUST surface the per-triple raw cargo binaries
    /// from `<worktree>/.det-tmp/target/<triple>/release/<bin>` alongside
    /// `dist/` artifacts, with the raw binaries getting a `target/...` map
    /// key prefix so the report distinguishes them from any same-basename
    /// `dist/` files. Closes the diagnostic gap where binary-level
    /// RUSTFLAGS / mtime drift was only observable through six layers of
    /// wrapper archives.
    ///
    /// When per-triple builds are present, the bare host
    /// `target/release/<bin>` is a non-shipped tooling byproduct (the
    /// man-page hook's `cargo run` output) and MUST be excluded — it hit
    /// intermittent windows-msvc codegen non-determinism and gating the
    /// release on it is a false positive.
    #[test]
    fn discover_artifacts_includes_raw_cargo_binaries() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();

        // dist artifact (existing surface)
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("anodize_0.3.0_linux_amd64.tar.gz"), b"archive").unwrap();

        // Cross-target build outputs
        let triple_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-unknown-linux-gnu")
            .join("release");
        std::fs::create_dir_all(&triple_release).unwrap();
        std::fs::write(triple_release.join("anodize"), b"raw-bin-linux").unwrap();
        // depfile must NOT be surfaced (cargo scratch).
        std::fs::write(triple_release.join("anodize.d"), b"depfile").unwrap();
        // `deps/` subdirectory must NOT be recursed (cargo scratch).
        std::fs::create_dir_all(triple_release.join("deps")).unwrap();
        std::fs::write(triple_release.join("deps").join("libfoo.rlib"), b"rlib").unwrap();

        // Windows-style triple with .exe
        let win_release = wt
            .join(".det-tmp")
            .join("target")
            .join("x86_64-pc-windows-msvc")
            .join("release");
        std::fs::create_dir_all(&win_release).unwrap();
        std::fs::write(win_release.join("anodize.exe"), b"raw-bin-windows").unwrap();
        // .pdb debug symbols must NOT be surfaced.
        std::fs::write(win_release.join("anodize.pdb"), b"pdb").unwrap();

        // Host build (no triple): target/release/anodize. With per-triple
        // builds present this is the non-shipped man-page-hook byproduct
        // and MUST be excluded.
        let host_release = wt.join(".det-tmp").join("target").join("release");
        std::fs::create_dir_all(&host_release).unwrap();
        std::fs::write(host_release.join("anodize"), b"raw-bin-host").unwrap();

        let artifacts = discover_artifacts(wt).expect("discover");
        let names: Vec<String> = artifacts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            names
                .iter()
                .any(|n| n == "anodize_0.3.0_linux_amd64.tar.gz"),
            "dist artifact missing: {names:?}"
        );
        // Only the per-triple `anodize` (linux) is shipped; the bare host
        // `target/release/anodize` is the man-page-hook byproduct and is
        // excluded when any per-triple build exists.
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "anodize").count(),
            1,
            "expected 1 `anodize` raw binary (linux triple only; host excluded), got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "anodize.exe"),
            "windows raw binary missing: {names:?}"
        );

        // Scratch files must NOT be surfaced.
        for forbidden in ["anodize.d", "anodize.pdb", "libfoo.rlib"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "cargo scratch `{forbidden}` leaked into discovery: {names:?}"
            );
        }

        // hash_artifacts must label the raw binaries with a `target/...`
        // map key so the report distinguishes them from `dist/`. Two
        // per-triple keys (linux + windows); the host binary is excluded.
        let map = hash_artifacts(wt, &artifacts).expect("hash");
        let target_keys: Vec<&String> = map.keys().filter(|k| k.starts_with("target/")).collect();
        assert_eq!(
            target_keys.len(),
            2,
            "expected 2 `target/...`-prefixed map keys (per-triple only), got: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        // The bare host `target/release/anodize` byproduct must NOT appear.
        assert!(
            !map.contains_key("target/release/anodize"),
            "non-shipped host byproduct `target/release/anodize` leaked into the comparison set: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        // Forward slashes regardless of host platform.
        for k in &target_keys {
            assert!(
                !k.contains('\\'),
                "raw-binary map key contains backslash: {k}"
            );
        }
        // Spot-check one key shape.
        assert!(
            target_keys
                .iter()
                .any(|k| { k.as_str() == "target/x86_64-unknown-linux-gnu/release/anodize" }),
            "expected `target/x86_64-unknown-linux-gnu/release/anodize` key, got: {target_keys:?}"
        );
        // Raw binaries get `build` stage attribution so the diagnostic
        // chain reads build → archive → checksum → sign.
        for k in &target_keys {
            assert_eq!(
                map.get(k.as_str()).map(|i| i.stage.as_str()),
                Some("build"),
                "raw binary `{k}` must be attributed to `build` stage"
            );
        }
    }

    /// When NO per-triple `<triple>/release/` directory exists, the bare
    /// host `target/release/<bin>` IS the shipped deliverable (a
    /// host-native consumer building with no `--target`) and MUST be
    /// surfaced. Complements `discover_artifacts_includes_raw_cargo_binaries`,
    /// which proves the byproduct is excluded once any per-triple build
    /// is present.
    #[test]
    fn discover_artifacts_includes_host_release_when_no_triple_present() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();

        // dist artifact (existing surface)
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("anodize_0.3.0_linux_amd64.tar.gz"), b"archive").unwrap();

        // Host build only — no `<triple>/release/` directory anywhere.
        let host_release = wt.join(".det-tmp").join("target").join("release");
        std::fs::create_dir_all(&host_release).unwrap();
        std::fs::write(host_release.join("anodize"), b"raw-bin-host").unwrap();
        // Scratch beside it must still be excluded.
        std::fs::write(host_release.join("anodize.d"), b"depfile").unwrap();
        std::fs::create_dir_all(host_release.join("deps")).unwrap();
        std::fs::write(host_release.join("deps").join("libfoo.rlib"), b"rlib").unwrap();

        let artifacts = discover_artifacts(wt).expect("discover");
        let names: Vec<String> = artifacts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            names
                .iter()
                .any(|n| n == "anodize_0.3.0_linux_amd64.tar.gz"),
            "dist artifact missing: {names:?}"
        );
        // The host binary is the shipped deliverable here and must appear.
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "anodize").count(),
            1,
            "host-native `target/release/anodize` must be surfaced when no triple build exists, got: {names:?}"
        );
        for forbidden in ["anodize.d", "libfoo.rlib"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "cargo scratch `{forbidden}` leaked into discovery: {names:?}"
            );
        }

        let map = hash_artifacts(wt, &artifacts).expect("hash");
        assert!(
            map.contains_key("target/release/anodize"),
            "expected `target/release/anodize` key (host-native deliverable), got: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// `prune_dump_to_drifted` MUST keep dumped bytes whose BASENAME
    /// matches a drift entry, even though the dumped path carries a
    /// `dist/` prefix. Regression: `DriftRow.artifact` for `dist/*`
    /// artifacts is the basename only (e.g. `"artifacts.json"`); the
    /// dumped file lives at `<run_dir>/dist/artifacts.json`. The prior
    /// shape compared only the full relative path, deleted every
    /// drifted file, and emitted an empty `drift-bins/**` upload —
    /// exactly the v0.3.0 CI failure where the operator had no way to
    /// inspect the differing artifact.
    #[test]
    fn prune_dump_to_drifted_keeps_files_matched_by_basename() {
        use anodizer_core::{
            AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow,
        };

        let tmp = tempfile::tempdir().unwrap();
        let dump_root = tmp.path();
        // Two runs, each with a drifted dist artifact + a deterministic
        // sibling that must be pruned.
        for run_idx in 0..2 {
            let run = dump_root.join(format!("run-{run_idx}"));
            std::fs::create_dir_all(run.join("dist")).unwrap();
            std::fs::write(run.join("dist/artifacts.json"), b"{}").unwrap();
            std::fs::write(run.join("dist/keep-me-not.tar.gz"), b"green").unwrap();
            // Raw cargo binary — matched by full rel path, not basename.
            let raw = run
                .join("target")
                .join("x86_64-unknown-linux-gnu")
                .join("release");
            std::fs::create_dir_all(&raw).unwrap();
            std::fs::write(raw.join("anodize"), b"binary").unwrap();
        }

        let report = DeterminismReport {
            schema_version: CURRENT_SCHEMA_VERSION,
            anodize_version: "0.3.0".into(),
            commit: "abc".into(),
            commit_timestamp: 0,
            runs: 2,
            stages_under_test: vec!["archive".into()],
            allowlist: AllowList::default(),
            artifacts: vec![],
            drift: vec![
                DriftRow {
                    artifact: "artifacts.json".into(),
                    hashes: vec!["sha256:a".into(), "sha256:b".into()],
                    differing_bytes_summary: None,
                },
                DriftRow {
                    artifact: "target/x86_64-unknown-linux-gnu/release/anodize".into(),
                    hashes: vec!["sha256:c".into(), "sha256:d".into()],
                    differing_bytes_summary: None,
                },
            ],
            drift_count: 2,
        };
        // ArtifactRow not required for prune; pad to satisfy invariants
        let _ = ArtifactRow {
            name: "noop".into(),
            path: "noop".into(),
            size_bytes: 0,
            stage: "unknown".into(),
            deterministic: true,
            nondeterministic_reason: None,
            hash: None,
            hashes: vec![],
        };

        prune_dump_to_drifted(dump_root, &report);

        for run_idx in 0..2 {
            let run = dump_root.join(format!("run-{run_idx}"));
            assert!(
                run.join("dist/artifacts.json").is_file(),
                "drifted dist artifact must survive prune (basename match)"
            );
            assert!(
                run.join("target/x86_64-unknown-linux-gnu/release/anodize")
                    .is_file(),
                "drifted raw binary must survive prune (rel-path match)"
            );
            assert!(
                !run.join("dist/keep-me-not.tar.gz").exists(),
                "non-drifted artifact must be pruned"
            );
        }
    }

    /// Sampler regression guard: `hash_artifacts` MUST emit a tail
    /// sample that closes the gap for mid-size artifacts. Previously
    /// files in `(HEAD, HEAD+TAIL]` carried an empty tail, leaving
    /// bytes `[HEAD..size]` unsampled — which is precisely where
    /// `artifacts.json` (~24 KiB) drifted in v0.3.0.
    #[test]
    fn hash_artifacts_samples_tail_for_mid_size_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        // 24 KiB content.
        let mut bytes = vec![0u8; 24 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i & 0xff) as u8;
        }
        std::fs::write(dist.join("artifacts.json"), &bytes).unwrap();

        let paths = discover_artifacts(wt).unwrap();
        let map = hash_artifacts(wt, &paths).unwrap();
        let info = map.get("artifacts.json").expect("artifacts.json must hash");
        assert_eq!(info.head_sample.len(), HEAD_SAMPLE_BYTES);
        assert_eq!(
            info.tail_sample.len(),
            bytes.len() - HEAD_SAMPLE_BYTES,
            "tail must cover bytes [HEAD..size] to close the gap"
        );
        // Round-trip: head + tail bytes must equal the original.
        let mut reconstructed = info.head_sample.clone();
        reconstructed.extend_from_slice(&info.tail_sample);
        assert_eq!(
            reconstructed, bytes,
            "head + tail must concatenate back to the original artifact"
        );
    }

    /// `discover_artifacts` must tolerate a missing `.det-tmp/target`
    /// (e.g. the harness has only just spawned and the child hasn't
    /// produced anything yet) — it shouldn't error out.
    #[test]
    fn discover_artifacts_tolerates_missing_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        // Just dist/, no .det-tmp/.
        let dist = wt.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("foo.tar.gz"), b"x").unwrap();
        let out = discover_artifacts(wt).expect("must not error on missing target dir");
        assert_eq!(out.len(), 1);
    }

    /// `hash_artifacts` must produce distinct map entries for same-basename
    /// files that live in different arch subdirectories under `dist/`.
    ///
    /// Regression: keying by basename collapses e.g.
    /// `dist/makeself/default/linux_amd64/anodizer` and
    /// `dist/makeself/default/linux_arm64/anodizer` — the second write
    /// overwrites the first. The current key is the dist-root-relative
    /// path, so both entries survive and carry their distinct hashes.
    #[test]
    fn hash_artifacts_distinguishes_same_basename_across_arch_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();

        let amd64_dir = wt
            .join("dist")
            .join("makeself")
            .join("default")
            .join("linux_amd64");
        let arm64_dir = wt
            .join("dist")
            .join("makeself")
            .join("default")
            .join("linux_arm64");
        std::fs::create_dir_all(&amd64_dir).unwrap();
        std::fs::create_dir_all(&arm64_dir).unwrap();

        std::fs::write(amd64_dir.join("anodizer"), b"amd64-bytes").unwrap();
        std::fs::write(arm64_dir.join("anodizer"), b"arm64-bytes").unwrap();

        let paths = discover_artifacts(wt).unwrap();
        let map = hash_artifacts(wt, &paths).unwrap();

        // Both entries must be present under their distinct relative paths.
        let amd64_key = "makeself/default/linux_amd64/anodizer";
        let arm64_key = "makeself/default/linux_arm64/anodizer";
        assert!(
            map.contains_key(amd64_key),
            "amd64 entry missing; map keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
        assert!(
            map.contains_key(arm64_key),
            "arm64 entry missing; map keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );

        // Hashes must differ — different bytes, different digests.
        let amd64_hash = &map[amd64_key].hash;
        let arm64_hash = &map[arm64_key].hash;
        assert_ne!(
            amd64_hash, arm64_hash,
            "distinct arch files must produce distinct hashes"
        );
    }
}
