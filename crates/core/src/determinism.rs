//! SOURCE_DATE_EPOCH seeding + compile-time / runtime allow-list state.
//!
//! `DeterminismState` is the per-run home for:
//! - `sde`: the SOURCE_DATE_EPOCH value (seconds since epoch) that every
//!   stage exports into subprocess env so artifacts have deterministic
//!   timestamps.
//! - `compile_time_allowlist`: artifact-name -> reason pairs known at
//!   build time for artifacts that are non-deterministic BY NATURE (a
//!   spec-mandated unique id or a tool's un-pinnable wall-clock anodizer
//!   cannot fix) — e.g. SBOM serial/namespace UUIDs, the flatpak OSTree
//!   commit. Formats proven byte-reproducible at a fixed SOURCE_DATE_EPOCH
//!   are NOT listed here; they are gated so a real regression is caught.
//! - `runtime_allowlist`: operator-supplied opt-outs via the
//!   `--allow-nondeterministic <name>=<reason>` CLI flag.
//!
//! Both lists are surfaced into the run-summary JSON
//! (`determinism_allowlist.compile_time` and `.runtime`) and the
//! per-artifact `PublishEvidence.nondeterministic` field. On collision
//! between the two lists, the compile-time reason wins on the per-
//! artifact field; both entries still appear in the report so the
//! audit trail is complete.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// MSVC-only RUSTFLAGS tokens that make a `*-pc-windows-msvc` binary
/// byte-reproducible across rebuilds.
///
/// This is the single source of truth for the Windows-MSVC determinism flag
/// set. It is referenced by both the build stage (per-target RUSTFLAGS for
/// every `anodizer release --reproducible` Windows build) and the
/// determinism harness's child-subprocess env construction. The static
/// `[target.*-pc-windows-msvc] rustflags` block in `.cargo/config.toml`
/// duplicates this list verbatim (a cargo config file cannot import a Rust
/// const); a comment there points back here so the two cannot silently drift.
///
/// Each token:
/// - `-C codegen-units=1` — single codegen unit so cross-CU function
///   ordering does not shuffle the object's symbol/section layout.
/// - `-C link-arg=/Brepro` — substitute the PE COFF `TimeDateStamp`
///   (offset 0x108) with a content hash instead of wall-clock time.
/// - `-C link-arg=/OPT:NOICF` — disable Identical COMDAT Folding, whose
///   fold decisions depend on input-file presentation order.
/// - `-C link-arg=/INCREMENTAL:NO` — disable incremental linking
///   (`/Brepro` is incompatible with it).
/// - `-C link-arg=/DEBUG:NONE` — emit no PDB / CodeView debug record.
/// - `-C strip=symbols` — drop the COFF symbol table.
///
/// `/Brepro` and the `/...` link args are MSVC-linker-only; applying them to
/// a non-MSVC target makes lld / ld error, so callers gate on
/// [`crate::target::is_windows_msvc`] (target-keyed) or
/// [`host_is_windows_msvc`] (host-keyed) before merging these in.
pub const MSVC_DETERMINISM_RUSTFLAGS: &[&str] = &[
    "-C codegen-units=1",
    "-C link-arg=/Brepro",
    "-C link-arg=/OPT:NOICF",
    "-C link-arg=/INCREMENTAL:NO",
    "-C link-arg=/DEBUG:NONE",
    "-C strip=symbols",
];

/// Merge [`MSVC_DETERMINISM_RUSTFLAGS`] into an existing space-delimited
/// RUSTFLAGS string, skipping any token already present so a value
/// inherited from config or a prior merge is not duplicated.
///
/// Returns the merged string. `base` may be empty.
pub fn merge_msvc_determinism_rustflags(base: &str) -> String {
    // Token-window comparison rather than a raw `contains`: each flag is a
    // multi-token unit (`-C link-arg=/Brepro`), and a substring test would
    // both false-positive on an unrelated prefix and miss the token-boundary
    // a real RUSTFLAGS split obeys.
    let mut out = base.trim().to_string();
    for &flag in MSVC_DETERMINISM_RUSTFLAGS {
        let flag_tokens: Vec<&str> = flag.split_whitespace().collect();
        let present = out
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(flag_tokens.len())
            .any(|w| w == flag_tokens.as_slice());
        if present {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(flag);
    }
    out
}

/// `true` when the detected host target triple is a Windows-MSVC triple.
///
/// Runtime host detection (via `rustc -vV` in
/// [`crate::partial::detect_host_target`]) — NOT a compile-time
/// `cfg!(windows)` check. The determinism harness binary may be built on a
/// different OS than the one it runs on (and a consumer may run
/// `anodizer check determinism` locally on Windows from any binary), so the
/// global-RUSTFLAGS `/Brepro` injection that keeps the host (`--target`-less)
/// build reproducible must key off the real running host, not the compile
/// target. A detection failure conservatively returns `false` (no MSVC flags
/// injected — correct for the overwhelmingly-common non-Windows host), and is
/// logged at `warn` so a swallowed `rustc` failure on a genuine windows-msvc
/// host (where skipping the flags silently regresses determinism) is auditable
/// rather than invisible.
pub fn host_is_windows_msvc() -> bool {
    match crate::partial::detect_host_target() {
        Ok(h) => crate::target::is_windows_msvc(&h),
        Err(e) => {
            // A bare `rustc` failure here is benign on the common non-Windows
            // host, but on windows-msvc it silently drops the reproducibility
            // flags — surface it so the skipped-determinism decision is traceable.
            tracing::warn!(
                error = %e,
                "host-target detection failed; treating host as non-windows-msvc and skipping MSVC determinism flags"
            );
            false
        }
    }
}

/// `true` when the detected host target triple is a macOS (Darwin) triple.
///
/// Runtime host detection (via `rustc -vV` in
/// [`crate::partial::detect_host_target`]) — NOT a compile-time
/// `cfg!(target_os = "macos")` check, for the same cross-host reason as
/// [`host_is_windows_msvc`]. Used by [`DeterminismState::seed_from_commit`]
/// to gate the `*.pkg` allow-list entry: only the macOS-native `pkgbuild`
/// path (which runs on the macOS determinism shard) is not yet proven
/// reproducible, whereas the Linux flat-package path (xar/mkbom/cpio) is
/// proven byte-stable and must stay gated. A detection failure
/// conservatively returns `false` — the overwhelmingly-common host is
/// Linux, where the `.pkg` is the proven xar path and must NOT be
/// allow-listed.
pub fn host_is_macos() -> bool {
    match crate::partial::detect_host_target() {
        Ok(h) => crate::target::is_darwin(&h),
        Err(e) => {
            // On a non-macOS host this is the right answer; on a genuine
            // macOS host a swallowed `rustc` failure would un-gate the
            // unproven pkgbuild `.pkg` and risk a false drift finding —
            // surface it so the decision is auditable.
            tracing::warn!(
                error = %e,
                "host-target detection failed; treating host as non-macOS (Linux xar .pkg path is gated, not allow-listed)"
            );
            false
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterminismState {
    pub sde: i64,
    pub compile_time_allowlist: Vec<(String, String)>,
    pub runtime_allowlist: Vec<(String, String)>,
}

impl DeterminismState {
    /// Seed from a commit timestamp (seconds since UNIX epoch). All built-
    /// in compile-time allow-list entries listed in the spec's contract
    /// table are added here.
    ///
    /// Returns `Err` when `commit_ts` is negative — a negative epoch would
    /// propagate a bogus `SOURCE_DATE_EPOCH` into child processes (where
    /// shells / build tools may misinterpret it) and almost always
    /// indicates a corrupted commit graph or a test passing a sentinel
    /// like `-1`. Fail-fast is the correct UX for a determinism API.
    ///
    /// ## Compile-time allow-list scope
    ///
    /// Each entry below corresponds to an artifact pattern the
    /// [`crate::determinism_report`] verification harness will actually
    /// see in `dist/`. Entries are matched by `*.ext` suffix or exact
    /// filename against the basename of every file the harness walks
    /// under the per-run worktree's `dist/` tree. Pattern names that do
    /// not match any real emitter output are dead code (silently never
    /// resolve) — keep this list aligned with what stages actually drop
    /// into `dist/`.
    ///
    /// Notably absent (and intentionally so):
    ///
    /// - `docker-manifest-descriptor` / `docker-image-blob`: the docker
    ///   stage is in [`crate::determinism_runner::SIDE_EFFECT_STAGES`]
    ///   and skipped by the harness; the only docker file that lands in
    ///   `dist/` is a `.digest` text file written by buildx (a
    ///   deterministic sha256). No need for an allow-list entry.
    /// - `apple-notarization-receipt`: the notarize stage mutates
    ///   existing artifacts in-place (staples) rather than emitting new
    ///   files; no separate "receipt" artifact lands in `dist/`.
    /// - The NSIS installer (`*-setup.exe` / `*_setup.exe`) is GATED, not
    ///   allow-listed. makensis honors `SOURCE_DATE_EPOCH` for the embedded
    ///   build timestamp, so two builds over identical inputs are
    ///   byte-identical — proven by
    ///   stage-nsis::nsis_setup_is_byte_reproducible_across_time. Under the
    ///   A′ shard routing the installer now lands in `dist/windows/` on the
    ///   Windows determinism shard (it builds the windows-msvc payload
    ///   binary), so the harness DOES see and byte-compare it. The classifier
    ///   keys on the `setup.exe` name tail so the installer is attributed to
    ///   `nsis` while the raw `anodize.exe` binary is not (see
    ///   `determinism_harness::artifacts::infer_stage_from_path`). A real
    ///   drift here is a regression, not an excused non-determinism.
    /// - The macOS `.app` bundle is GATED, not allow-listed. It is pure
    ///   file assembly (Info.plist + binary copy) at a fixed mtime /
    ///   `SOURCE_DATE_EPOCH`, so it is byte-reproducible — proven by
    ///   stage-appbundle::appbundle_is_byte_reproducible_across_time. Under
    ///   A′ it is produced on the macOS determinism shard (which holds the
    ///   darwin payload binary), so the harness byte-compares it directly.
    pub fn seed_from_commit(commit_ts: i64) -> Result<Self> {
        if commit_ts < 0 {
            anyhow::bail!(
                "commit_ts must be non-negative (got {}); a corrupted commit graph or future-bug? \
                 Negative SOURCE_DATE_EPOCH would propagate to child processes and be \
                 misinterpreted by shells/build tools.",
                commit_ts
            );
        }
        // Installer formats whose non-determinism is intrinsic (a tool's
        // wall-clock or spec-mandated unique id anodizer cannot pin) AND
        // empirically proven by a two-build `cmp` test. Reproducible formats
        // are NOT here — they are gated so a real regression is caught.
        // Each entry carries its `.sha256` sidecar: the sidecar hashes a
        // non-deterministic source so it is itself non-deterministic, but
        // not an independent finding worth surfacing.
        //
        // `.deb` / `.rpm` are allow-listed BECAUSE the real config GPG-signs
        // them (`deb.signature.key_file` / `rpm.signature.key_file` driven by
        // `GPG_KEY_PATH`), and the determinism harness provisions an ephemeral
        // GPG key so signing runs. The GPG signature embeds a non-pinnable
        // creation time / randomized salt that is not byte-reproducible even at
        // a fixed SOURCE_DATE_EPOCH (the same intrinsic-signature class as the
        // cosign `.sig` files). The package BODY is fully byte-reproducible —
        // proven by stage-nfpm::signed_{deb,rpm}_body_is_byte_reproducible_across_time
        // — and the signature is verified cryptographically (gpg --verify), not
        // by byte-equality. An earlier UNSIGNED reproducibility proof had
        // removed them from this list; that missed the signed real-config path.
        //
        // Conspicuously gated, each byte-identical across two builds at a fixed
        // SOURCE_DATE_EPOCH (the value the harness exports), so the harness must
        // count any drift as a real regression:
        //   - `.crate`, `.snap`, the Linux xar `.pkg` — UNSIGNED, so the whole
        //     artifact must be byte-reproducible.
        //   - `.apk` — SIGNED, but unlike the deb/rpm GPG signature nfpm's apk
        //     RSA signature is DETERMINISTIC (PKCS#1, no salt / no embedded
        //     timestamp), so the whole signed artifact is byte-reproducible —
        //     proven by stage-nfpm::signed_apk_is_byte_reproducible_across_time.
        //     Hence gated, NOT allow-listed (contrast `.deb`/`.rpm`, whose GPG
        //     signature is non-reproducible).
        // See the gating tests cited per format below in the unit-test module.
        let mut installer_allow: Vec<(&str, &str)> = vec![
            (
                "*.msi",
                "WiX candle/light regenerates a random SummaryInformation PackageCode GUID and stamps wall-clock Created/LastModified into the MSI summary-information stream, independent of SOURCE_DATE_EPOCH (wixtoolset/issues#8978); not byte-reproducible — proven by stage-msi::msi_is_byte_reproducible_across_time. Built natively on the windows determinism shard under A′.",
            ),
            (
                "*.dmg",
                "hdiutil writes a fresh per-segment SegmentID GUID into the UDIF koly trailer every run and pins no SOURCE_DATE_EPOCH; not byte-reproducible — proven by stage-dmg::dmg_is_byte_reproducible_across_time. Built natively on the macos determinism shard under A′.",
            ),
            (
                "*.flatpak",
                "flatpak build-bundle wraps an OSTree commit whose metadata (commit object timestamp + per-object headers) is not byte-stable across runs even at a fixed SOURCE_DATE_EPOCH; empirically confirmed non-reproducible via two-build cmp",
            ),
            (
                "*.deb",
                "nfpm GPG-signs the deb (_gpgorigin ar member); the signature embeds a non-pinnable creation time / randomized salt and is not byte-reproducible even at a fixed SOURCE_DATE_EPOCH. The package body (debian-binary, control.tar.gz, data.tar.gz) IS byte-reproducible — gated by stage-nfpm::signed_deb_body_is_byte_reproducible_across_time — and the signature is verified cryptographically (gpg --verify), not by byte-equality.",
            ),
            (
                "*.rpm",
                "nfpm GPG-signs the rpm (RPM signature header); the signature embeds a non-pinnable creation time / randomized salt and is not byte-reproducible even at a fixed SOURCE_DATE_EPOCH. The package body (main header + cpio payload, i.e. everything after the signature header) IS byte-reproducible — gated by stage-nfpm::signed_rpm_body_is_byte_reproducible_across_time — and the signature is verified cryptographically (gpg --verify), not by byte-equality.",
            ),
        ];
        // `*.pkg` is host-keyed. Only one producer runs per determinism
        // shard: Linux runs the xar/mkbom/cpio flat-package path, proven
        // byte-reproducible (stage-pkg::test_flat_pkg_is_byte_reproducible_across_time)
        // — so on Linux the `.pkg` is GATED, never allow-listed. macOS runs
        // the native `pkgbuild` path, not yet proven; on a macOS host the
        // `.pkg` is allow-listed pending the macos-shard proof.
        if host_is_macos() {
            installer_allow.push((
                "*.pkg",
                "macOS-native pkgbuild stamps a wall-clock xar TOC and ignores SOURCE_DATE_EPOCH; not byte-reproducible — proven by stage-pkg::native_pkgbuild_pkg_is_byte_reproducible_across_time. Built natively on the macos determinism shard under A′ (the Linux xar/mkbom/cpio .pkg path is proven reproducible and gated, never allow-listed)",
            ));
        }
        // SBOMs embed identifiers that are non-reproducible by nature:
        // CycloneDX carries a random `serialNumber` UUID plus a generation
        // `metadata.timestamp`, and SPDX carries a `documentNamespace` UUID
        // plus a `created` timestamp. syft does not honor SOURCE_DATE_EPOCH
        // for the document timestamp and (per the CycloneDX/SPDX specs) the
        // serial/namespace must be unique per document, so two runs over
        // byte-identical inputs still produce differing SBOM bytes. These
        // SBOMs are excluded from the reproducibility
        // guarantee. Surfaced in the report, excluded from `drift_count`.
        // Extensions mirror `infer_stage_from_path`'s `sbom` classifier.
        let sbom_allow: &[(&str, &str)] = &[
            (
                "*.cdx.json",
                "CycloneDX SBOM embeds a random serialNumber UUID and a generation timestamp (syft does not honor SOURCE_DATE_EPOCH for it); not byte-reproducible across runs",
            ),
            (
                "*.spdx.json",
                "SPDX SBOM embeds a documentNamespace UUID and a created timestamp; not byte-reproducible across runs",
            ),
            (
                "*.sbom.json",
                "SBOM document embeds a per-document unique identifier and generation timestamp; not byte-reproducible across runs",
            ),
        ];
        let mut compile_time_allowlist: Vec<(String, String)> = Vec::new();
        for (pattern, reason) in installer_allow.iter().chain(sbom_allow) {
            compile_time_allowlist.push(((*pattern).into(), (*reason).into()));
            compile_time_allowlist.push((
                format!("{}.sha256", pattern),
                format!("derivative of {pattern}: {reason}"),
            ));
        }
        // `artifacts.json` and the combined `*_checksums.txt` are NOT
        // blanket-allow-listed here. A blanket entry masks real regressions:
        // it excuses the manifest even when a GATED (byte-reproducible)
        // member drifted. They are handled instead by the determinism
        // harness's aggregate registry (see [`AggregateKind`]), which excuses
        // an aggregate's drift IFF every differing member is itself
        // allow-listed — the transitive-derivation rule — and otherwise
        // surfaces the offending member as a real regression.

        Ok(Self {
            sde: commit_ts,
            compile_time_allowlist,
            runtime_allowlist: Vec::new(),
        })
    }

    /// Export SOURCE_DATE_EPOCH onto a `std::process::Command` so
    /// child subprocesses (cargo, tar, sbom tools, etc.) see the
    /// reproducible epoch.
    pub fn export_env(&self, cmd: &mut Command) {
        cmd.env("SOURCE_DATE_EPOCH", self.sde.to_string());
    }

    /// Resolve the allow-list reason for an artifact name. Compile-time
    /// entries win on collision per the spec's "Operator escape /
    /// Precedence on collision" section. Returns None when the artifact
    /// is not in either list.
    pub fn resolve_reason(&self, artifact: &str) -> Option<&str> {
        // Compile-time first
        for (name, reason) in &self.compile_time_allowlist {
            if matches_artifact_pattern(name, artifact) {
                return Some(reason.as_str());
            }
        }
        // Then runtime
        for (name, reason) in &self.runtime_allowlist {
            if matches_artifact_pattern(name, artifact) {
                return Some(reason.as_str());
            }
        }
        None
    }

    /// Append a runtime allow-list entry. Caller is the CLI flag
    /// handler for `--allow-nondeterministic <name>=<reason>`.
    pub fn append_runtime(&mut self, artifact: String, reason: String) {
        self.runtime_allowlist.push((artifact, reason));
    }
}

/// Simple glob: `*.ext` matches any artifact ending in `.ext`;
/// exact-match otherwise. Avoids pulling a globbing crate for this
/// narrow case.
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

// ---------------------------------------------------------------------------
// Aggregate-artifact registry — the transitive-derivation rule
// ---------------------------------------------------------------------------
//
// An *aggregate* is a file whose bytes are a deterministic function of a set
// of *member* artifacts (a checksums file lists `<digest>  <name>` per member;
// `artifacts.json` records each member's path + digest). Such a file drifts
// whenever ANY listed member drifts — including the allow-listed
// non-deterministic ones (signed deb/rpm, SBOMs, signatures). A blanket
// allow-list on the aggregate would therefore MASK a real regression in a
// gated (byte-reproducible) member.
//
// The registry lets the determinism harness reconstruct an aggregate's members
// from both runs and excuse its drift IFF every *differing* member is itself
// allow-listed; any differing member that is NOT allow-listed is surfaced as a
// real regression. The two ids below are the compile-time coupling anchor: each
// producer references its id (mirroring the `MSVC_DETERMINISM_RUSTFLAGS` ↔
// `.cargo/config.toml` coupling above), so a producer and its registry entry
// cannot silently drift apart.

/// Aggregate id of the combined `*_checksums.txt` file produced by
/// `anodizer_stage_checksum`'s `refresh_combined_checksums`. Referenced by
/// that producer so the registry entry and the emitter share one symbol.
pub const COMBINED_CHECKSUMS_AGGREGATE_ID: &str = "combined-checksums";

/// Aggregate id of the `artifacts.json` dist manifest produced by the CLI's
/// `write_artifacts_and_metadata`. Referenced by that producer so the
/// registry entry and the emitter share one symbol.
pub const ARTIFACTS_MANIFEST_AGGREGATE_ID: &str = "artifacts-manifest";

/// A class of aggregate artifact recognized by the determinism harness.
///
/// Implementors parse the aggregate's bytes into a `unit_key -> member`
/// map. A *unit* is one line / one manifest entry; its `unit_key` is
/// content-sensitive (it folds in the recorded digest) so a value-change
/// for a stable member identity surfaces as an add/remove pair across
/// runs. The `member` is the artifact basename the harness resolves
/// against the determinism allow-list.
pub trait AggregateKind {
    /// Stable id (one of the `*_AGGREGATE_ID` consts), the compile-time
    /// coupling anchor shared with the producing stage.
    fn id(&self) -> &'static str;

    /// `true` when `name` (a harness artifact key, possibly nested under a
    /// per-crate subdirectory) is an instance of this aggregate.
    fn matches(&self, name: &str) -> bool;

    /// Parse `bytes` into `unit_key -> member`. Errors (fail-closed) when
    /// the bytes are not valid for this aggregate — the caller treats a
    /// parse failure as real drift, never an excuse.
    fn members_by_unit(&self, bytes: &[u8]) -> Result<BTreeMap<String, String>>;
}

/// Basename (last `/`- or `\`-separated component), lowercased — used for
/// aggregate name matching across single-crate / lockstep / per-crate dist
/// layouts (per-crate nests the manifest under `<crate>/`).
fn aggregate_basename(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_lowercase()
}

/// The combined `<name>_<version>_checksums.txt` / `sha256sums` file: one
/// `<digest>  <filename>` line per primary artifact.
pub struct CombinedChecksums;

impl AggregateKind for CombinedChecksums {
    fn id(&self) -> &'static str {
        COMBINED_CHECKSUMS_AGGREGATE_ID
    }

    /// SECONDARY heuristic only. The authoritative signal that a file is a
    /// combined checksums aggregate is its `artifacts.json` entry carrying
    /// the [`crate::artifact::COMBINED_CHECKSUM_META`] = `"true"` marker —
    /// resolved by [`combined_checksum_members_from_manifest`]. This
    /// filename-suffix match is the fallback for callers that lack manifest
    /// context (or for a combined file emitted with a conventional name);
    /// it deliberately does NOT recognize an operator-renamed combined file
    /// such as `SHA512SUMS`, which only the marker can identify.
    fn matches(&self, name: &str) -> bool {
        let base = aggregate_basename(name);
        base.ends_with("checksums.txt")
            || base.ends_with("sha256sums")
            || base.ends_with("sha256sum")
    }

    fn members_by_unit(&self, bytes: &[u8]) -> Result<BTreeMap<String, String>> {
        let text =
            std::str::from_utf8(bytes).context("combined checksums file is not valid UTF-8")?;
        let mut out = BTreeMap::new();
        for raw in text.lines() {
            let line = raw.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue;
            }
            // GNU coreutils format is `<digest>  <filename>` (two spaces).
            // The filename is the rightmost field; it is the basename the
            // refresh-combined writer emits, so it resolves directly against
            // the allow-list.
            let filename = line
                .rsplit("  ")
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .with_context(|| format!("checksum line missing a filename field: {line:?}"))?;
            // unit_key = the whole line: a changed digest yields a new line,
            // surfaced as an add/remove of the same member across runs.
            out.insert(line.to_string(), filename.to_string());
        }
        Ok(out)
    }
}

/// The `artifacts.json` dist manifest: a JSON array of entries, each with a
/// `path` and (after checksumming) a recorded digest in `metadata`.
pub struct ArtifactsManifest;

impl AggregateKind for ArtifactsManifest {
    fn id(&self) -> &'static str {
        ARTIFACTS_MANIFEST_AGGREGATE_ID
    }

    fn matches(&self, name: &str) -> bool {
        aggregate_basename(name) == crate::dist::ARTIFACTS_JSON
    }

    fn members_by_unit(&self, bytes: &[u8]) -> Result<BTreeMap<String, String>> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).context("parsing artifacts.json dist manifest")?;
        let entries = value
            .as_array()
            .context("artifacts.json dist manifest must be a JSON array")?;
        let mut out = BTreeMap::new();
        for entry in entries {
            let path = entry
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("artifacts.json entry missing a string `path` field")?;
            let member = path
                .rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(path)
                .to_string();
            // Content token: the recorded digest if present, else the entire
            // canonical entry — so ANY drift in the entry (digest, size,
            // metadata) is attributed to this member rather than silently
            // excused.
            let token = entry
                .get("metadata")
                .and_then(|m| {
                    m.get("sha256")
                        .or_else(|| m.get("SHA256"))
                        .or_else(|| m.get("Checksum"))
                })
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| serde_json::to_string(entry).unwrap_or_default());
            // unit_key folds the digest into the path identity so a
            // value-change (same path, new digest) surfaces as an add/remove
            // pair judged by `member`'s allow-list status. U+001F is the ASCII
            // unit separator — it cannot occur in a path or hex digest.
            let unit_key = format!("{path}\u{1f}{token}");
            out.insert(unit_key, member);
        }
        Ok(out)
    }
}

/// Every recognized aggregate kind, in priority order.
pub fn aggregate_kinds() -> Vec<Box<dyn AggregateKind>> {
    vec![Box::new(CombinedChecksums), Box::new(ArtifactsManifest)]
}

/// Return the [`AggregateKind`] that recognizes `name`, if any.
pub fn aggregate_kind_for(name: &str) -> Option<Box<dyn AggregateKind>> {
    aggregate_kinds().into_iter().find(|k| k.matches(name))
}

/// Parse an `artifacts.json` manifest and return the basenames of every entry
/// flagged as a combined checksums file via the
/// [`crate::artifact::COMBINED_CHECKSUM_META`] = [`crate::artifact::COMBINED_CHECKSUM_VALUE`]
/// marker.
///
/// This is the AUTHORITATIVE recognizer for the combined-checksums aggregate:
/// the checksum stage stamps that marker onto the combined file's manifest
/// entry regardless of the operator's chosen filename, so a renamed
/// `SHA512SUMS` (which [`CombinedChecksums::matches`]'s filename suffixes
/// cannot catch) is still recognized. Callers union these basenames with the
/// filename-suffix heuristic. Returns an error (fail-closed) when the bytes
/// are not a JSON array of objects — the caller treats that as real drift.
pub fn combined_checksum_members_from_manifest(bytes: &[u8]) -> Result<BTreeSet<String>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("parsing artifacts.json dist manifest")?;
    let entries = value
        .as_array()
        .context("artifacts.json dist manifest must be a JSON array")?;
    let mut out = BTreeSet::new();
    for entry in entries {
        let is_combined = entry
            .get("metadata")
            .and_then(|m| m.get(crate::artifact::COMBINED_CHECKSUM_META))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| v == crate::artifact::COMBINED_CHECKSUM_VALUE);
        if !is_combined {
            continue;
        }
        let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let base = path
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(path);
        out.insert(base.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msvc_rustflags_const_matches_cargo_config() {
        // The static `.cargo/config.toml` block is the only other home for
        // this flag set (it can't import a Rust const). Keep them aligned:
        // the config writes each `-C <arg>` as a two-element TOML array pair,
        // so the joined form must equal this const verbatim.
        let cargo_config = include_str!("../../../.cargo/config.toml");
        for &flag in MSVC_DETERMINISM_RUSTFLAGS {
            // `-C codegen-units=1` -> `"-C", "codegen-units=1"`
            let (lead, rest) = flag.split_once(' ').expect("each flag is two tokens");
            let toml_pair = format!("\"{lead}\", \"{rest}\"");
            assert!(
                cargo_config.contains(&toml_pair),
                ".cargo/config.toml must carry `{toml_pair}` to stay aligned with \
                 MSVC_DETERMINISM_RUSTFLAGS; the two drifted"
            );
        }
    }

    #[test]
    fn merge_msvc_into_empty_yields_full_set() {
        let merged = merge_msvc_determinism_rustflags("");
        for &flag in MSVC_DETERMINISM_RUSTFLAGS {
            assert!(
                merged.contains(flag),
                "merged set must contain `{flag}`. got={merged}"
            );
        }
        assert!(
            merged.contains("/Brepro"),
            "the COFF TimeDateStamp fix must be present"
        );
    }

    #[test]
    fn merge_msvc_preserves_existing_flags_and_appends() {
        let base = "-C linker=link.exe --remap-path-prefix=/w=/build";
        let merged = merge_msvc_determinism_rustflags(base);
        assert!(
            merged.starts_with(base),
            "existing flags must be preserved verbatim. got={merged}"
        );
        assert!(
            merged.contains("-C link-arg=/Brepro"),
            "MSVC flags must be appended. got={merged}"
        );
    }

    #[test]
    fn merge_msvc_is_idempotent_no_duplicate_brepro() {
        let once = merge_msvc_determinism_rustflags("");
        let twice = merge_msvc_determinism_rustflags(&once);
        assert_eq!(
            once, twice,
            "merging an already-merged set must not duplicate flags"
        );
        assert_eq!(
            twice.matches("/Brepro").count(),
            1,
            "/Brepro must appear exactly once after a double merge. got={twice}"
        );
    }

    #[test]
    fn host_is_windows_msvc_matches_real_host() {
        // On this Linux CI box the real host triple is non-MSVC, so the
        // runtime probe must report false (the bug class this guards is a
        // compile-time check that would report the wrong answer on a
        // cross-built binary).
        let expected = crate::partial::detect_host_target()
            .map(|h| crate::target::is_windows_msvc(&h))
            .unwrap_or(false);
        assert_eq!(host_is_windows_msvc(), expected);
    }

    #[test]
    fn sde_from_commit_timestamp_is_idempotent() {
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s.sde, 1_715_000_000);
        let s2 = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s, s2);
    }

    #[test]
    fn cargo_crate_is_gated_not_allowlisted() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // `cargo package` is byte-identical across two builds at a fixed
        // SOURCE_DATE_EPOCH (the harness exports it; cargo normalizes the
        // source tarball — sorted paths, pinned mtime). The `.crate` must be
        // gated so a real regression is caught, NOT allow-listed. The
        // per-crate-rendered name must not resolve in any of the three modes.
        assert!(s.resolve_reason("anodizer-0.2.1.crate").is_none());
        assert!(s.resolve_reason("anodizer-core-0.11.3.crate").is_none());
        // And its sidecar must not be allow-listed as a derivative either.
        assert!(
            s.resolve_reason("anodizer-core-0.11.3.crate.sha256")
                .is_none()
        );
    }

    #[test]
    fn rpm_and_deb_are_allowlisted_due_to_gpg_signing() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The real config GPG-signs both formats; the signature is intrinsically
        // non-byte-reproducible even at a fixed SOURCE_DATE_EPOCH while the body
        // IS reproducible (proven by stage-nfpm::signed_{deb,rpm}_body_is_byte_-
        // reproducible_across_time). Both — and their sidecars — are allow-listed.
        let rpm = s.resolve_reason("foo-1.0.rpm").expect("matches *.rpm");
        assert!(
            rpm.contains("signature") || rpm.contains("GPG"),
            "rpm reason must cite the signature: {rpm}"
        );
        let deb = s
            .resolve_reason("foo_1.0_amd64.deb")
            .expect("matches *.deb");
        assert!(
            deb.contains("signature") || deb.contains("GPG"),
            "deb reason must cite the signature: {deb}"
        );
        assert!(s.resolve_reason("foo-1.0.rpm.sha256").is_some());
        assert!(s.resolve_reason("foo_1.0_amd64.deb.sha256").is_some());
    }

    #[test]
    fn snap_is_gated_not_allowlisted() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // snapcraft pack (mksquashfs) honors SOURCE_DATE_EPOCH for the
        // squashfs mod_time, so .snap is byte-identical across two builds
        // (proven by stage-snapcraft::snap_is_byte_reproducible_across_time).
        // Gated, never allow-listed as "defense-in-depth".
        assert!(s.resolve_reason("probe_1.2.3_amd64.snap").is_none());
        assert!(s.resolve_reason("probe_1.2.3_amd64.snap.sha256").is_none());
    }

    #[test]
    fn apk_is_signed_but_gated() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // nfpm RSA-signs the apk, but the signature is deterministic (PKCS#1,
        // no salt / no embedded timestamp) so the whole signed artifact is
        // byte-identical across two builds at a fixed SOURCE_DATE_EPOCH —
        // proven by stage-nfpm::signed_apk_is_byte_reproducible_across_time.
        // It must be GATED (real drift = regression), NOT allow-listed like
        // the GPG-signed deb/rpm whose signature is non-reproducible.
        assert!(s.resolve_reason("foo_1.0_amd64.apk").is_none());
        assert!(s.resolve_reason("foo_1.0_amd64.apk.sha256").is_none());
    }

    #[test]
    fn compile_time_allowlist_resolves_for_flatpak() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // flatpak build-bundle wraps a non-byte-stable OSTree commit; the
        // harness must not count a `.flatpak` as drift.
        let reason = s
            .resolve_reason("anodizer_0.9.1_linux_amd64.flatpak")
            .expect("matches *.flatpak");
        assert!(reason.contains("OSTree"));
        // The `.sha256` sidecar over a non-deterministic bundle is itself
        // non-deterministic — allow-listed as a derivative.
        assert!(
            s.resolve_reason("anodizer_0.9.1_linux_amd64.flatpak.sha256")
                .is_some()
        );
    }

    #[test]
    fn pkg_allowlist_is_host_keyed() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // `.pkg` is allow-listed ONLY on a macOS host (the unproven native
        // pkgbuild path); on a Linux host the `.pkg` is the proven xar/mkbom/
        // cpio flat-package path and must be gated. The seed reflects the
        // running host, so the resolved verdict must match it.
        let resolved = s.resolve_reason("anodizer-0.2.1.pkg").is_some();
        assert_eq!(
            resolved,
            host_is_macos(),
            "`.pkg` must be allow-listed iff the host is macOS (Linux xar path is gated)"
        );
        // This CI box is Linux, so confirm the gated direction concretely:
        // the proven Linux path must NOT be excused.
        if !host_is_macos() {
            assert!(
                s.resolve_reason("anodizer-0.2.1.pkg").is_none(),
                "Linux xar .pkg path is proven reproducible and must be gated"
            );
            assert!(s.resolve_reason("anodizer-0.2.1.pkg.sha256").is_none());
        }
    }

    #[test]
    fn appbundle_is_gated_not_allowlisted() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The macOS `.app` bundle is pure file assembly at a fixed mtime /
        // SOURCE_DATE_EPOCH, so it is byte-reproducible — proven by
        // stage-appbundle::appbundle_is_byte_reproducible_across_time. Under A′
        // it is produced on the macOS determinism shard and byte-compared, so
        // it must be GATED (real drift = regression), NEVER allow-listed.
        assert!(s.resolve_reason("anodizer_arm64.app").is_none());
        assert!(s.resolve_reason("anodizer_amd64.app").is_none());
        // A `.app` is a directory, but a `.sha256` over it (if one is ever
        // emitted) must likewise not be excused as a derivative.
        assert!(s.resolve_reason("anodizer_arm64.app.sha256").is_none());
    }

    #[test]
    fn nsis_installer_is_gated_not_allowlisted() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // makensis honors SOURCE_DATE_EPOCH, so the NSIS installer is byte-
        // reproducible — proven by
        // stage-nsis::nsis_setup_is_byte_reproducible_across_time. Under A′ it
        // lands in dist/windows/ on the Windows determinism shard and is byte-
        // compared, so it must be GATED, NEVER allow-listed. Neither the
        // configured `-setup.exe` name nor the stage-default `_setup.exe` tail
        // may resolve to an allow-list reason.
        assert!(s.resolve_reason("anodizer_x64-setup.exe").is_none());
        assert!(s.resolve_reason("anodizer_x64_setup.exe").is_none());
        assert!(
            s.resolve_reason("anodizer_arm64-setup.exe.sha256")
                .is_none()
        );
    }

    #[test]
    fn raw_windows_binary_is_not_allowlisted_or_misclassified() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The raw windows binary `anodizer.exe` is the load-bearing build
        // output — it must be byte-reproducible (the /Brepro RUSTFLAGS make it
        // so) and is therefore GATED. It must never be swept into an installer
        // allow-list class by a bare `*.exe` pattern (there is none; only the
        // intrinsically-non-reproducible native installers are allow-listed).
        assert!(s.resolve_reason("anodizer.exe").is_none());
        assert!(s.resolve_reason("anodizer.exe.sha256").is_none());
    }

    #[test]
    fn msi_and_dmg_reasons_cite_pending_shard_proof() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // These run on the windows/macos shards (not provable on this Linux
        // box); their reasons must cite the sibling pending shard tests
        // rather than a "deferred to follow-up" hedge.
        let msi = s
            .resolve_reason("anodizer-0.2.1.msi")
            .expect("matches *.msi");
        assert!(
            msi.contains("msi_is_byte_reproducible_across_time"),
            "msi reason must cite the pending shard test: {msi}"
        );
        let dmg = s
            .resolve_reason("anodizer-0.2.1.dmg")
            .expect("matches *.dmg");
        assert!(
            dmg.contains("dmg_is_byte_reproducible_across_time"),
            "dmg reason must cite the pending shard test: {dmg}"
        );
    }

    #[test]
    fn compile_time_allowlist_resolves_for_sbom_documents() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // syft-generated CycloneDX/SPDX SBOMs carry a random serial/namespace
        // UUID + generation timestamp and can never be byte-identical across
        // runs; the harness must not count them as drift.
        for name in [
            "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json",
            "cfgd-0.4.0-linux-amd64.tar.gz.spdx.json",
            "cfgd-0.4.0-linux-amd64.tar.gz.sbom.json",
        ] {
            assert!(
                s.resolve_reason(name).is_some(),
                "SBOM document {name} must be allow-listed"
            );
        }
    }

    #[test]
    fn compile_time_allowlist_resolves_for_sbom_checksum_sidecars() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The `.sha256` sidecar hashes the non-deterministic SBOM, so it is
        // itself non-deterministic — allow-listed as a derivative.
        let reason = s
            .resolve_reason("cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256")
            .expect("matches *.cdx.json.sha256");
        assert!(reason.contains("derivative of"));
    }

    #[test]
    fn artifacts_manifest_is_not_blanket_allowlisted() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // The dist manifest is NO LONGER blanket-allow-listed — a blanket
        // entry masked drift in gated (byte-reproducible) members. It is now
        // handled by the aggregate registry's transitive-derivation rule.
        assert!(
            s.resolve_reason("artifacts.json").is_none(),
            "artifacts.json must not be blanket-excused; the registry judges it per-member"
        );
        // The combined checksums file is likewise registry-driven, never
        // blanket-allow-listed.
        assert!(s.resolve_reason("anodizer_0.12.0_checksums.txt").is_none());
        assert!(s.resolve_reason("config.json").is_none());
        assert!(s.resolve_reason("metadata.json").is_none());
    }

    #[test]
    fn aggregate_registry_matches_checksums_and_manifest() {
        // Combined checksums: bare, per-crate-nested, and sha256sums spellings.
        assert_eq!(
            aggregate_kind_for("anodizer_0.12.0_checksums.txt").map(|k| k.id()),
            Some(COMBINED_CHECKSUMS_AGGREGATE_ID)
        );
        assert_eq!(
            aggregate_kind_for("anodizer-core/anodizer-core_0.12.0_checksums.txt").map(|k| k.id()),
            Some(COMBINED_CHECKSUMS_AGGREGATE_ID)
        );
        assert_eq!(
            aggregate_kind_for("SHA256SUMS").map(|k| k.id()),
            Some(COMBINED_CHECKSUMS_AGGREGATE_ID)
        );
        // artifacts.json (root + per-crate-nested).
        assert_eq!(
            aggregate_kind_for("artifacts.json").map(|k| k.id()),
            Some(ARTIFACTS_MANIFEST_AGGREGATE_ID)
        );
        assert_eq!(
            aggregate_kind_for("anodizer-core/artifacts.json").map(|k| k.id()),
            Some(ARTIFACTS_MANIFEST_AGGREGATE_ID)
        );
        // A per-artifact `.sha256` split sidecar is NOT an aggregate.
        assert!(aggregate_kind_for("anodizer_0.12.0_linux_amd64.tar.gz.sha256").is_none());
        // metadata.json is a tracked primary, never an aggregate.
        assert!(aggregate_kind_for("metadata.json").is_none());
    }

    #[test]
    fn combined_checksums_members_by_unit_round_trips() {
        let bytes = b"aaaa  foo_1.0_amd64.deb\nbbbb  bar-1.0.tar.gz\n\ncccc  baz.cdx.json\n";
        let units = CombinedChecksums.members_by_unit(bytes).expect("parses");
        let members: std::collections::BTreeSet<&str> =
            units.values().map(String::as_str).collect();
        assert_eq!(
            members,
            ["foo_1.0_amd64.deb", "bar-1.0.tar.gz", "baz.cdx.json"]
                .into_iter()
                .collect()
        );
        // Each unit_key is the full line (content-sensitive): a changed digest
        // yields a distinct key.
        assert!(units.contains_key("aaaa  foo_1.0_amd64.deb"));
        // Blank lines are skipped (3 members, not 4).
        assert_eq!(units.len(), 3);
    }

    #[test]
    fn artifacts_manifest_members_by_unit_round_trips_and_keys_on_digest() {
        let json = br#"[
          {"kind":"archive","path":"./dist/foo.tar.gz","name":"foo.tar.gz",
           "metadata":{"sha256":"deadbeef"}},
          {"kind":"linux_package","path":"./dist/nfpm/foo_1.0_amd64.deb","name":"foo_1.0_amd64.deb",
           "metadata":{"Checksum":"sha256:cafef00d"}}
        ]"#;
        let units = ArtifactsManifest.members_by_unit(json).expect("parses");
        let members: std::collections::BTreeSet<&str> =
            units.values().map(String::as_str).collect();
        assert_eq!(
            members,
            ["foo.tar.gz", "foo_1.0_amd64.deb"].into_iter().collect()
        );
        // The recorded digest is folded into the unit_key, so re-hashing the
        // same path with a different digest produces a different key.
        let drifted = br#"[
          {"kind":"archive","path":"./dist/foo.tar.gz","name":"foo.tar.gz",
           "metadata":{"sha256":"00000000"}}
        ]"#;
        let drifted_units = ArtifactsManifest.members_by_unit(drifted).expect("parses");
        let original_key = units
            .keys()
            .find(|k| k.starts_with("./dist/foo.tar.gz"))
            .unwrap();
        assert!(
            !drifted_units.contains_key(original_key),
            "a changed digest must yield a new unit_key (value-change ⇒ add/remove)"
        );
    }

    #[test]
    fn aggregate_members_by_unit_fail_closed_on_garbage() {
        // Non-UTF-8 checksums bytes and non-array JSON both error so the
        // harness fails closed (treats the aggregate as real drift).
        assert!(
            CombinedChecksums
                .members_by_unit(&[0xff, 0xfe, 0x00])
                .is_err()
        );
        assert!(ArtifactsManifest.members_by_unit(b"not json").is_err());
        assert!(ArtifactsManifest.members_by_unit(b"{}").is_err());
    }

    #[test]
    fn aggregate_ids_are_distinct_and_stable() {
        assert_ne!(
            COMBINED_CHECKSUMS_AGGREGATE_ID,
            ARTIFACTS_MANIFEST_AGGREGATE_ID
        );
        assert_eq!(CombinedChecksums.id(), COMBINED_CHECKSUMS_AGGREGATE_ID);
        assert_eq!(ArtifactsManifest.id(), ARTIFACTS_MANIFEST_AGGREGATE_ID);
    }

    #[test]
    fn combined_checksum_marker_recognizes_operator_renamed_file() {
        // The combined file is named `SHA512SUMS` — a name the filename-suffix
        // heuristic deliberately misses — but its manifest entry carries the
        // `combined = "true"` marker, so the authoritative recognizer finds it.
        let manifest = br#"[
          {"kind":"archive","path":"./dist/foo.tar.gz","name":"foo.tar.gz",
           "metadata":{"sha256":"aaaa"}},
          {"kind":"checksum","path":"./dist/SHA512SUMS","name":"SHA512SUMS",
           "metadata":{"combined":"true"}}
        ]"#;
        let markers = combined_checksum_members_from_manifest(manifest).expect("parses");
        assert!(
            markers.contains("SHA512SUMS"),
            "marker recognizer must flag the renamed combined file: {markers:?}"
        );
        // The filename-suffix fallback alone does NOT recognize it.
        assert!(!CombinedChecksums.matches("SHA512SUMS"));
        assert!(aggregate_kind_for("SHA512SUMS").is_none());
        // A per-artifact split checksum sidecar (no `combined` marker) is NOT
        // flagged.
        assert!(!markers.contains("foo.tar.gz"));
    }

    #[test]
    fn combined_checksum_marker_fails_closed_on_non_array() {
        assert!(combined_checksum_members_from_manifest(b"not json").is_err());
        assert!(combined_checksum_members_from_manifest(b"{}").is_err());
        // An empty array is valid and yields no markers.
        assert!(
            combined_checksum_members_from_manifest(b"[]")
                .expect("empty array parses")
                .is_empty()
        );
    }

    #[test]
    fn nondeterministic_allowlist_compile_time_wins_on_collision() {
        let mut s = DeterminismState::seed_from_commit(0).expect("non-negative");
        // Runtime entry shadowing a compile-time pattern. Compile-time
        // wins so the report shows the deeper rationale. `*.flatpak` is a
        // permanently-allow-listed compile-time pattern (intrinsically
        // non-reproducible OSTree commit), so it is the stable collision
        // anchor regardless of host.
        s.append_runtime(
            "*.flatpak".into(),
            "operator escape (wrong runtime reason)".into(),
        );
        let reason = s
            .resolve_reason("anodizer_0.9.1_linux_amd64.flatpak")
            .unwrap();
        assert!(
            reason.contains("OSTree"),
            "compile-time reason takes precedence"
        );
    }

    #[test]
    fn nondeterministic_allowlist_serializes_with_both_categories() {
        let mut s = DeterminismState::seed_from_commit(0).expect("non-negative");
        s.append_runtime("foo.bin".into(), "tool-bug-1234".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("compile_time_allowlist"));
        assert!(json.contains("runtime_allowlist"));
        assert!(json.contains("foo.bin"));
    }

    #[test]
    fn export_env_sets_source_date_epoch() {
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        let mut cmd = Command::new("true");
        s.export_env(&mut cmd);
        let env_vars: Vec<(_, _)> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
            .collect();
        let sde_entry = env_vars.iter().find(|(k, _)| k == "SOURCE_DATE_EPOCH");
        assert!(sde_entry.is_some());
        assert_eq!(sde_entry.unwrap().1, "1715000000");
    }

    #[test]
    fn resolve_reason_returns_none_for_unrecognized() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        assert!(s.resolve_reason("unrelated.txt").is_none());
    }

    #[test]
    fn seed_from_commit_accepts_zero() {
        // Epoch zero (1970-01-01) is a legitimate sentinel — some
        // determinism modes anchor SDE to UNIX epoch when the commit
        // graph isn't usable. Must not be rejected.
        let s = DeterminismState::seed_from_commit(0).expect("zero is non-negative");
        assert_eq!(s.sde, 0);
    }

    #[test]
    fn seed_from_commit_accepts_positive() {
        // Typical real-world commit timestamp.
        let s = DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        assert_eq!(s.sde, 1_715_000_000);
    }

    #[test]
    fn seed_from_commit_rejects_negative() {
        let err = DeterminismState::seed_from_commit(-1).expect_err("negative must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-negative") && msg.contains("-1"),
            "error must name the bad input and the constraint: {msg}"
        );
    }
}
