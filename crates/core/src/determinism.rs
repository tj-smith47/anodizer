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

use anyhow::Result;
use serde::{Deserialize, Serialize};
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
    /// - `*.exe-nsis`: makensis writes plain `.exe` files into
    ///   `dist/windows/`; the suffix `.exe-nsis` matches nothing the
    ///   harness ever sees. NSIS-built `.exe` files only appear when
    ///   running on Windows (or under Wine), and operators can use the
    ///   runtime `--allow-nondeterministic <name>=<reason>` flag on
    ///   those releases rather than hard-coding a dead sentinel here.
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
                "WiX candle/light embeds a non-pinnable build timestamp in the MSI summary-information stream; pending proof by msi_is_byte_reproducible_across_time on the windows determinism shard",
            ),
            (
                "*.dmg",
                "hdiutil writes a wall-clock HFS+/APFS volume creation date the macOS host will not pin to SOURCE_DATE_EPOCH; pending proof by dmg_is_byte_reproducible_across_time on the macos determinism shard",
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
                "nfpm GPG-signs the rpm (RPM header signature); the signature embeds a non-pinnable creation time / randomized salt and is not byte-reproducible even at a fixed SOURCE_DATE_EPOCH. The package body (rpm2cpio payload) IS byte-reproducible — gated by stage-nfpm::signed_rpm_body_is_byte_reproducible_across_time — and the signature is verified cryptographically (gpg --verify), not by byte-equality.",
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
                "macOS-native pkgbuild emits a non-pinnable build timestamp; pending proof by native_pkgbuild_pkg_is_byte_reproducible_across_time on the macos determinism shard (the Linux xar/mkbom/cpio .pkg path is proven reproducible and gated, never allow-listed)",
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
        // `artifacts.json` is anodize's own dist manifest: it records the
        // `size` and `sha256` of every produced artifact. Its byte-stability
        // is exactly the conjunction of all indexed artifacts', so it can
        // only drift when (a) a real build output drifted — already caught
        // directly on that artifact — or (b) an allow-listed non-deterministic
        // artifact (SBOM, signature) drifted — intentionally excluded. It
        // therefore carries no independent determinism signal; comparing its
        // bytes only re-surfaces drift already accounted for. Exact-match so
        // no other `.json` is swept in.
        compile_time_allowlist.push((
            "artifacts.json".into(),
            "anodize dist manifest aggregating every artifact's size+digest \
             (including allow-listed non-deterministic SBOMs/signatures); a derivative \
             signal — each indexed artifact is drift-checked independently"
                .into(),
        ));

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
    fn compile_time_allowlist_resolves_for_artifacts_manifest() {
        let s = DeterminismState::seed_from_commit(0).expect("non-negative");
        assert!(
            s.resolve_reason("artifacts.json").is_some(),
            "the dist manifest aggregates non-deterministic artifact sizes/digests"
        );
        // Exact-match: must not sweep in unrelated `.json` files.
        assert!(s.resolve_reason("config.json").is_none());
        assert!(s.resolve_reason("metadata.json").is_none());
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
