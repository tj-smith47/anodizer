use super::*;

/// Parse a comma-separated stage subset (`--stages=build,archive,...`).
///
/// Returns `Err` on unknown tokens — silently dropping typos like
/// `--stages=archve,checksum` (note the missing `i`) is a UX trap that
/// quietly under-verifies the release; the operator typed a stage they
/// expected to be exercised. Empty / whitespace-only tokens (e.g. a
/// trailing comma) are tolerated. Both an absent flag and an empty
/// selection (`--stages=""`) fall back to [`default_stages_for_host`] —
/// the OS-native partition the harness builds when no filter is given.
pub(super) fn parse_stages(s: Option<&str>) -> Result<Vec<StageId>, String> {
    // Umbrella selector for every installer-family stage. Operators
    // type `--stages=installers` to exercise the full set in one shot;
    // individual family stages (`msi`, `nsis`, ...) remain available
    // for narrower runs. Delegating to the harness's
    // `installer_detect::installer_stages` keeps the CLI parser and
    // harness gate consulting the same source of truth.
    match s {
        None => Ok(default_stages_for_host()),
        Some(list) => {
            let mut parsed: Vec<StageId> = Vec::new();
            let mut unknown: Vec<String> = Vec::new();
            for tok in list.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    // Tolerate trailing / empty tokens (e.g.
                    // `archive,checksum,`); the operator clearly meant
                    // the named stages and the empty slot is noise.
                    continue;
                }
                if tok == "installers" {
                    parsed.extend(installer_stages());
                } else if let Some(stage) = StageId::from_token(tok) {
                    parsed.push(stage);
                } else {
                    unknown.push(tok.to_string());
                }
            }
            if !unknown.is_empty() {
                // The legal vocabulary is the enum itself (via `as_str`) plus
                // the `installers` umbrella — built from `StageId::iter()` so a
                // new variant joins the hint without a hand edit here.
                let mut known: Vec<&str> = StageId::iter().map(StageId::as_str).collect();
                known.push("installers");
                return Err(format!(
                    "--stages contained unknown stage(s): {}. Known stages: {}.",
                    unknown.join(", "),
                    known.join(", ")
                ));
            }
            // De-dup while preserving insertion order so
            // `--stages=installers,msi` (umbrella followed by an
            // individual member) doesn't list `msi` twice in
            // `stages_under_test`. The first mention wins, matching
            // the operator's typed intent.
            let mut seen: std::collections::HashSet<StageId> = std::collections::HashSet::new();
            let mut deduped: Vec<StageId> = Vec::with_capacity(parsed.len());
            for stage in parsed {
                if seen.insert(stage) {
                    deduped.push(stage);
                }
            }
            Ok(if deduped.is_empty() {
                default_stages_for_host()
            } else {
                deduped
            })
        }
    }
}

/// The OS-appropriate stage partition the harness builds when `--stages` is
/// absent — "no filter" means "byte-verify everything this host can natively
/// produce", never a minimal subset that silently under-covers a release.
///
/// This encodes the partition that USED to live as a hand-written
/// `det_stages:` key per shard in `.github/workflows/determinism.yml`; the
/// per-OS "what is appropriate to build here" decision is intrinsic to the
/// tool, not a CI concern, so it belongs in the harness. `--stages=` remains
/// a USER filter layered on top.
///
/// ## Why the partition is per-OS (payload-binary routing)
///
/// The determinism harness is sharded by host precisely because one host
/// cannot cross-compile every target's binary, and a produce-stage emits
/// nothing on a shard that lacks its payload binary — so each installer must
/// run on the shard that natively builds what it packages:
///
/// - `appbundle` / `dmg` / `pkg` → **macOS** (need the darwin binary). On
///   macOS `appbundle` precedes `dmg`/`pkg` so their `use: appbundle` finds a
///   source `.app`.
/// - `msi` / `nsis` → **Windows** (need the windows-msvc binary).
/// - `docker` / `appimage` / `flatpak` / `nfpm` / `makeself` / `snapcraft` /
///   `srpm` → **Linux**.
///
/// Routing an installer to a shard without its payload binary is how these
/// formats silently shipped in NO release for so long (they were listed only
/// on the linux-only ubuntu shard, which produces no darwin/windows binary).
///
/// ## Per-format reproducibility verdict
///
/// The harness byte-compares the GATED formats and counts any drift as a
/// regression; the ALLOWLISTED ones are intrinsically non-reproducible (see
/// `anodizer_core::DeterminismState::seed_from_commit`) and excluded from
/// `drift_count` while still surfaced in the report:
///
/// - `install-script` — **GATED**: the `curl | sh` installer is derived from
///   configured release intent (targets + flagship crate) and only written to
///   disk — no external tool, no read of produced binaries — so it is
///   byte-identical on every shard by construction and its merge-dedup across
///   shards is a no-op.
/// - `appbundle` — **GATED**: pure file assembly, byte-reproducible
///   (`appbundle_is_byte_reproducible_across_time`).
/// - `nsis` — **GATED**: `makensis` honors `SOURCE_DATE_EPOCH`, byte-
///   reproducible (`nsis_setup_is_byte_reproducible_across_time`).
/// - `dmg` — **ALLOWLISTED**: `hdiutil` writes a fresh UDIF koly SegmentID
///   GUID per run; native, non-reproducible.
/// - `pkg` — **ALLOWLISTED**: macOS-native `pkgbuild` stamps a wall-clock xar
///   TOC and ignores `SOURCE_DATE_EPOCH`.
/// - `msi` — **ALLOWLISTED**: WiX regenerates a random PackageCode GUID plus
///   Created/LastModified (wixtoolset/issues#8978).
///
/// ## Tool gate
///
/// The tool gate (see [`crate::determinism_harness`]'s
/// `gate_installer_stages` / the docker fork) further prunes any stage in
/// this default whose backing tool is absent on the host. By default a
/// host-default stage warn-skips so the harness stays usable everywhere (only
/// an explicitly typed `--stages=<stage>` hard-fails). Under CI's
/// `--require-tools` the WHOLE resolved set is promoted to hard-fail, so a
/// missing OS-native producer tool fails the shard rather than silently
/// under-covering the release.
///
/// `cargo-package` is intentionally NOT in this default: it is a harness-only
/// cross-platform probe of `cargo package` byte-stability, not a shipped
/// artifact, so it stays opt-in via `--stages=cargo-package`.
///
/// This returns the config-INDEPENDENT OS partition; the resolved default
/// applied when `--stages` is absent is this set intersected with the
/// config-configured producers — see [`host_default_for_config`].
pub(super) fn default_stages_for_host() -> Vec<StageId> {
    let mut stages = ALWAYS_ON_STAGES.to_vec();
    let host_os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        ""
    };
    for token in anodizer_core::env_preflight::os_native_producer_tokens(host_os) {
        // Core's producer table is keyed by the same canonical token
        // vocabulary as `StageId::as_str`, so an absent mapping is an internal
        // invariant breach (a producer token with no `StageId`) — not operator
        // input — and must fail loud rather than silently drop a producer.
        stages.push(
            StageId::from_token(token).expect("core producer token must map to a StageId variant"),
        );
    }
    stages
}

/// Stages produced for ANY config — they carry no installer tool and emit
/// nothing when unconfigured, so the host default keeps them unconditionally
/// rather than gating on config. Everything else in
/// [`default_stages_for_host`] is a config-gated producer pruned by
/// [`host_default_for_config`].
pub(super) const ALWAYS_ON_STAGES: &[StageId] = &[
    StageId::Build,
    StageId::Source,
    StageId::Upx,
    StageId::Archive,
    StageId::InstallScript,
    StageId::Sbom,
    StageId::Sign,
    StageId::Checksum,
];

/// The resolved DEFAULT stage set (the `--stages`-absent path): the OS-native
/// partition ([`default_stages_for_host`]) with each config-gated producer
/// kept only when the loaded config actually configures it.
///
/// Determinism can only byte-verify artifacts the config PRODUCES, so a
/// generic consumer whose `.anodizer.yaml` has no `flatpaks:` block must not
/// get `flatpak` in its default — otherwise `--require-tools` would hard-fail
/// on a missing `flatpak-builder` for an artifact that project never builds.
/// The configured-producer set is the core SSOT
/// [`anodizer_core::env_preflight::configured_producer_stages`] (the same
/// `Config`/`CrateConfig` fields the pipeline's stage gates read); the
/// always-on base ([`ALWAYS_ON_STAGES`]) is never gated.
///
/// `config` must already have `apply_defaults` run on it (producers declared
/// under `defaults:` materialize onto crates). `None` (config failed to load)
/// falls back to the full OS partition — the conservative "do not silently
/// under-verify" choice; a genuine config-load failure surfaces from the
/// pipeline itself.
///
/// Only the DEFAULT path is intersected: an EXPLICIT `--stages=<x>` is the
/// operator's typed intent and is left exactly as parsed (it still hard-fails
/// on a missing tool, config notwithstanding).
pub(super) fn host_default_for_config(
    config: Option<&anodizer_core::config::Config>,
) -> Vec<StageId> {
    let full = default_stages_for_host();
    let Some(config) = config else {
        return full;
    };
    let configured = anodizer_core::env_preflight::configured_producer_stages(config);
    full.into_iter()
        .filter(|s| ALWAYS_ON_STAGES.contains(s) || configured.contains(s.as_str()))
        .collect()
}

/// Whether `--stages` carries an EXPLICIT operator selection — at least one
/// non-blank token. `None`, `Some("")`, and `Some(",, ")` are all non-explicit
/// (they resolve to the host default). The single predicate behind both the
/// stage-set resolution ([`resolve_stages`]) and the explicit-stages hard-fail
/// set, so the two cannot disagree about what counts as "operator typed it".
pub(super) fn is_explicit_stage_selection(stages_arg: Option<&str>) -> bool {
    matches!(stages_arg, Some(list) if list.split(',').any(|t| !t.trim().is_empty()))
}

/// Resolve the stage set under test from the `--stages` argument and the
/// loaded (defaults-applied) config.
///
/// An EXPLICIT selection (≥1 real token) is the operator's typed intent and
/// passes straight through [`parse_stages`], unchanged by config. An absent or
/// all-empty `--stages` resolves to the config-intersected host default
/// ([`host_default_for_config`]).
pub(super) fn resolve_stages(
    stages_arg: Option<&str>,
    config: Option<&anodizer_core::config::Config>,
) -> Result<Vec<StageId>, String> {
    if is_explicit_stage_selection(stages_arg) {
        parse_stages(stages_arg)
    } else {
        Ok(host_default_for_config(config))
    }
}

/// Parse a comma-separated triple list (`--targets=x86_64-...,aarch64-...`).
///
/// Thin wrapper over `commands::helpers::parse_csv_list` that supplies
/// the `--targets`-shaped error hint. Unlike `--stages=<csv>`, there is
/// no closed vocabulary to validate against here — the legal set is
/// whatever appears in the project's `.anodizer.yaml` `targets` list,
/// and that's resolved later in the pipeline.
pub(super) fn parse_targets(s: Option<&str>) -> Result<Option<Vec<String>>, String> {
    crate::commands::helpers::parse_csv_list(
        s,
        "--targets=x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu",
    )
}
