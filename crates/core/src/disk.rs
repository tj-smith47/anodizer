//! Disk-headroom instrumentation and a peak-measured fail-fast guard for
//! the determinism harness.
//!
//! The harness rebuilds the full release pipeline `runs` times in a fresh
//! worktree per iteration. On the macOS shard each run builds both darwin
//! arches for the universal binary, then archive/sbom/sign/**dmg**/pkg —
//! and `hdiutil` (the dmg builder) stages a temp read/write image before
//! compressing, so the dmg stage is the disk high-water mark. When the
//! runner is gradually exhausted across runs, the pipeline limps into an
//! opaque `hdiutil: create failed - No space left on device` with zero
//! disk awareness.
//!
//! ## Why a PEAK measurement, not a net delta
//!
//! A run's footprint is not its *net* on-disk residue. The per-triple
//! build-intermediate reclaim (`stage-build`'s
//! `free_cargo_build_intermediates`) and the worktree drop settle a run's
//! consumption down AFTER the dmg stage has already peaked. So a
//! between-runs net delta (`free_before_run_k − free_before_run_{k+1}`)
//! systematically EXCLUDES the mid-dmg peak it is meant to bound, and a
//! guard built on it under-provisions and limps into the same ENOSPC.
//!
//! The build runs in a child `anodizer release` subprocess, so the harness
//! cannot read the mid-run peak inline. Instead it runs a background
//! [`FreeSpaceSampler`] for the duration of each run's build: a thread that
//! polls [`available_bytes`] on a short interval, recording the MINIMUM
//! free space it observes. `peak_consumed_run_k = free_before_run_k −
//! min_free_during_run_k` is then a *measured* bound on what that run
//! actually needed at its worst moment. The guard gates run-1..N on the
//! MAX peak observed across all prior runs × a modest safety factor — a
//! real measured bound, not a guess.
//!
//! ## Guarantee, honestly stated
//!
//! - **run-0** has no prior peak to measure, so it is gated by the
//!   absolute floor ([`DEFAULT_ABS_FLOOR_BYTES`]) ALONE — a liveness
//!   backstop, not a peak guarantee. The sampler still records run-0's
//!   peak (emitted at verbose) to gate run-1.
//! - **run-1..N** are gated on the measured peak of every prior run — the
//!   observed failure was run-1, and peak-gating it is the fix.
//!
//! Everything here is subprocess-free: free space via [`fs4`]'s
//! `statvfs`/`GetDiskFreeSpaceExW` wrapper, directory size via a `std::fs`
//! recursive walk, mounted volumes via `std::fs::read_dir("/Volumes")` on
//! macOS. Nothing mutates disk state; it only observes and decides. The
//! footprint/threshold math ([`RunPeak`], [`evaluate_headroom`]) takes
//! plain byte counts so it is unit-testable without a real low-disk host.

use crate::env_source::{EnvSource, ProcessEnvSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// `1 GiB` in bytes — the unit the harness reports headroom in.
const GIB: u64 = 1024 * 1024 * 1024;

/// Default absolute free-space floor, in bytes, required before run-0
/// starts and as a backstop for run-1..N.
///
/// This is a minimal **liveness backstop**, NOT a peak guarantee:
/// "enough free for a build to even start". It must NOT be sized to any
/// one workload's peak. A blanket absolute floor is workload-blind — it
/// cannot tell a tiny project apart from the macOS universal+dmg
/// pipeline — so sizing it to the heavy case would false-positive on
/// every small real project running `anodizer check determinism` on a
/// normal machine (and on the harness's own integration tests, which run
/// minimal workspaces with only a few GiB free).
///
/// The real heavy-workload protection is the MEASURED-PEAK gate on
/// run-1..N: run-0 starts on the reclaimed macOS shard (~101 GiB free),
/// clears this `2 GiB` floor trivially, and the [`FreeSpaceSampler`]
/// measures run-0's true peak to gate run-1 — the run that actually
/// exhausted the disk. If a future instrumented CI run shows run-0 itself
/// is marginal, raise the floor for that shard via [`ABS_FLOOR_ENV`]
/// rather than baking a macOS peak into the cross-workload default.
pub const DEFAULT_ABS_FLOOR_BYTES: u64 = 2 * GIB;

/// Default multiplier applied to a run's MEASURED peak consumption when
/// gating subsequent runs. `1.3×` is slack ABOVE the observed peak —
/// absorbing sampling jitter (the sampler may miss the exact instant of
/// maximum consumption between polls) and minor run-to-run variance —
/// without demanding headroom a correctly-sized runner can't supply.
///
/// Because the base figure is now a real peak (not a net→peak guess), the
/// factor stays modest; raising it compensates only for measurement
/// granularity, not for a structural under-estimate.
pub const DEFAULT_SAFETY_FACTOR: f64 = 1.3;

/// Default interval between [`FreeSpaceSampler`] polls.
///
/// Tradeoff: responsiveness (catching the dmg-stage high-water mark, which
/// can spike and recede within a couple of seconds) vs syscall frequency.
/// `available_bytes` is a single cheap `statvfs`/`GetDiskFreeSpaceExW`
/// syscall, so 2-3× more samples is negligible cost — and for a feature
/// that must NEVER silently under-report the peak, a `0.5s` cadence
/// materially lowers the probability that the high-water mark falls
/// entirely between two polls. The `DEFAULT_SAFETY_FACTOR` (1.3×) remains
/// the cushion for any residual under-measure between samples. No env
/// override — the const is the single tuning point.
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Env override for [`DEFAULT_ABS_FLOOR_BYTES`], expressed in **whole
/// GiB** (e.g. `ANODIZER_DET_DISK_FLOOR_GIB=60`). A zero / non-positive /
/// malformed / overflowing value falls back to the default — `0` must
/// never disable the guard.
pub const ABS_FLOOR_ENV: &str = "ANODIZER_DET_DISK_FLOOR_GIB";

/// Env override for [`DEFAULT_SAFETY_FACTOR`] (e.g.
/// `ANODIZER_DET_DISK_SAFETY_FACTOR=2.0`). A non-finite or non-positive
/// value falls back to the default.
pub const SAFETY_FACTOR_ENV: &str = "ANODIZER_DET_DISK_SAFETY_FACTOR";

/// Resolve the absolute free-space floor (bytes) from the environment,
/// falling back to [`DEFAULT_ABS_FLOOR_BYTES`]. The override is read in
/// whole GiB so an operator strengthening reclaim can raise the floor
/// without computing byte counts.
///
/// `0` is rejected (it would make `required = 0` and silently disable the
/// guard for an entire CI history) along with malformed input and any
/// value whose `× GiB` would overflow `u64`.
///
/// Thin production wrapper over [`abs_floor_bytes_from_env_with`] reading
/// the real process env via [`ProcessEnvSource`].
pub fn abs_floor_bytes_from_env() -> u64 {
    abs_floor_bytes_from_env_with(&ProcessEnvSource)
}

/// [`abs_floor_bytes_from_env`] parameterized over an [`EnvSource`] so the
/// parse/validation branches are testable by injecting a [`MapEnvSource`]
/// — no process-env mutation, no test-isolation marker, race-free by
/// construction. An absent key reads as `None` and falls through to the
/// default.
///
/// [`MapEnvSource`]: crate::env_source::MapEnvSource
pub fn abs_floor_bytes_from_env_with(env: &dyn EnvSource) -> u64 {
    env.var(ABS_FLOOR_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&gib| gib > 0)
        .and_then(|gib| gib.checked_mul(GIB))
        .unwrap_or(DEFAULT_ABS_FLOOR_BYTES)
}

/// Resolve the safety factor from the environment, falling back to
/// [`DEFAULT_SAFETY_FACTOR`]. Non-finite or non-positive overrides are
/// rejected (they would invert or zero the guard).
///
/// Thin production wrapper over [`safety_factor_from_env_with`] reading
/// the real process env via [`ProcessEnvSource`].
pub fn safety_factor_from_env() -> f64 {
    safety_factor_from_env_with(&ProcessEnvSource)
}

/// [`safety_factor_from_env`] parameterized over an [`EnvSource`] so the
/// parse/validation branches are testable by injecting a [`MapEnvSource`]
/// without mutating the process env.
///
/// [`MapEnvSource`]: crate::env_source::MapEnvSource
pub fn safety_factor_from_env_with(env: &dyn EnvSource) -> f64 {
    env.var(SAFETY_FACTOR_ENV)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(DEFAULT_SAFETY_FACTOR)
}

/// Format a byte count as a human-readable `GiB` string with one decimal
/// (`101.2 GiB`). Used in routine disk log lines so the units are uniform.
pub fn format_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Format a byte count as `GiB` with two decimals plus the raw byte count
/// (`12.04 GiB (12929498972 bytes)`). Used in the abort message so a
/// near-miss (`required 12.04 / available 11.96`) never renders as
/// `12.0 vs 12.0`; the exact bytes make the decision auditable.
pub fn format_gib_exact(bytes: u64) -> String {
    format!("{:.2} GiB ({bytes} bytes)", bytes as f64 / GIB as f64)
}

/// Free (available-to-this-user) bytes on the filesystem backing `path`.
///
/// Returns `None` when the probe fails (path doesn't exist yet, an
/// unsupported platform, a transient stat error). Callers treat `None`
/// as "headroom unknown" and degrade to a no-op rather than abort — the
/// guard must never *manufacture* a failure from a probe gap.
///
/// `fs4::available_space` reports the space available to an unprivileged
/// process (statvfs `f_bavail` × `f_frsize` on unix, the caller-quota
/// figure from `GetDiskFreeSpaceExW` on Windows) — the correct number for
/// "will this write succeed", as opposed to total free including
/// root-reserved blocks.
pub fn available_bytes(path: &Path) -> Option<u64> {
    // Honor the documented "missing path → None" contract on every
    // platform. `statvfs` (unix) already fails for an absent `path`, but
    // `GetDiskFreeSpaceExW` (windows) resolves the *parent volume* of a
    // missing path and reports its free space — which both contradicts the
    // contract and masks a not-yet-mounted target (it would report the
    // parent FS, not the intended mount). Gate on existence first so an
    // absent path degrades to "headroom unknown" uniformly, never a
    // manufactured parent-volume reading. A probe error (`Err`) is likewise
    // treated as unknown. The harness always probes the worktree root
    // (created before any run), so the existence check is satisfied in
    // practice and costs one extra stat at the sampler's 0.5s cadence.
    if !path.try_exists().unwrap_or(false) {
        return None;
    }
    fs4::available_space(path).ok()
}

/// Recursively sum the size of every regular file under `dir`.
///
/// Symlinks are not followed (their target may live on another volume or
/// form a cycle); their own entry contributes nothing. A missing `dir`
/// (e.g. `dist/` before the first produce-stage) sums to `0` rather than
/// erroring — this is a diagnostic aid, not a correctness gate. Per-entry
/// stat errors are skipped so one unreadable file can't blank the whole
/// total.
pub fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let entries = match std::fs::read_dir(&cur) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            // `entry.metadata()` does NOT traverse symlinks (it is a
            // `symlink_metadata`): a symlinked dir is recorded as a (tiny)
            // link, never recursed into.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Names of `anodize`-related mounts under `/Volumes` (macOS only).
///
/// **Diagnostic only.** `hdiutil` mounts the dmg's transient r/w image at
/// `/Volumes/<volname>` while copying the staging tree in; the `stage-dmg`
/// crate already `hdiutil detach -force`es a stale same-named mount before
/// `create`, so name COLLISION is handled there. This list exists purely
/// to make a *leaked* mount visible in the harness log (its disk
/// occupancy is not reclaimed here) so a recurrence is diagnosable.
/// Returns an empty vector off macOS or when `/Volumes` is unreadable.
#[cfg(target_os = "macos")]
pub fn mounted_volumes() -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir("/Volumes") {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Non-macOS stub: there is no `/Volumes` concept, so the mount list is
/// always empty. Kept so callers need no `cfg` of their own.
#[cfg(not(target_os = "macos"))]
pub fn mounted_volumes() -> Vec<String> {
    Vec::new()
}

/// Background free-space sampler for one determinism run's build.
///
/// Spawned just before a run's build subprocess starts and
/// [`stop`](Self::stop)ped when it returns. While alive, a thread polls
/// [`available_bytes`] every `interval` and records the MINIMUM free space
/// it observes into a shared atomic. The minimum, subtracted from the
/// free space measured before the run, is the run's measured PEAK
/// consumption — the mid-dmg high-water mark a between-runs net delta
/// cannot see.
///
/// If the probe is unavailable (`available_bytes` returns `None`), the
/// sampler records nothing and [`stop`](Self::stop) yields `None`; the
/// guard then has no peak for that run and falls back to the floor, never
/// a manufactured value.
pub struct FreeSpaceSampler {
    /// Stop signal. The worker blocks on `recv_timeout(interval)`; a send
    /// (or sender-drop) wakes it IMMEDIATELY, so `stop()` never has to
    /// wait out a full poll interval before the thread joins.
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    /// Minimum free bytes observed; `u64::MAX` sentinel until the first
    /// successful probe so a probe-gap run yields `None`.
    min_free: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl FreeSpaceSampler {
    /// Sentinel meaning "no successful probe yet". Distinguishes a genuine
    /// `0 bytes free` reading from a run where the probe never succeeded.
    const NO_SAMPLE: u64 = u64::MAX;

    /// Start sampling free space on the volume backing `vol`, polling every
    /// `interval`.
    pub fn start(vol: &Path, interval: Duration) -> Self {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        let (stop_tx, stop_rx) = channel::<()>();
        let min_free = Arc::new(AtomicU64::new(Self::NO_SAMPLE));
        let min_t = Arc::clone(&min_free);
        let vol: PathBuf = vol.to_path_buf();
        let handle = std::thread::spawn(move || {
            // Take an immediate reading so a build shorter than one
            // interval still records a sample, then poll on a timeout that
            // a stop-send interrupts at once.
            loop {
                if let Some(free) = available_bytes(&vol) {
                    min_t.fetch_min(free, Ordering::Relaxed);
                }
                // `recv_timeout` returns `Ok`/`Disconnected` the instant
                // `stop()` sends or drops the sender → prompt shutdown;
                // `Timeout` means the interval elapsed → take another
                // sample. Sampling cadence stays at `interval`; only stop
                // responsiveness improves.
                match stop_rx.recv_timeout(interval) {
                    Err(RecvTimeoutError::Timeout) => continue,
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            stop_tx: Some(stop_tx),
            min_free,
            handle: Some(handle),
        }
    }

    /// Stop sampling, join the thread, and return the minimum free space
    /// observed (or `None` if the probe never succeeded).
    pub fn stop(mut self) -> Option<u64> {
        self.signal_and_join();
        match self.min_free.load(Ordering::Relaxed) {
            Self::NO_SAMPLE => None,
            v => Some(v),
        }
    }

    /// Signal the worker to stop and reap it. Idempotent: dropping the
    /// sender wakes a `recv_timeout` immediately, and a `take`n handle
    /// guards against a double-join.
    fn signal_and_join(&mut self) {
        // Drop the sender to wake the worker at once (its `recv_timeout`
        // returns `Disconnected`); an explicit `send` is unnecessary.
        self.stop_tx = None;
        if let Some(h) = self.handle.take() {
            // A panic in the sampler thread is non-fatal to the harness —
            // a failed join just means "no peak measured", same as a probe
            // gap; the guard falls back to the floor.
            let _ = h.join();
        }
    }
}

impl Drop for FreeSpaceSampler {
    fn drop(&mut self) {
        // Defensive: if `stop()` was not called (early return / `?`), make
        // sure the thread is signalled and reaped rather than detached.
        self.signal_and_join();
    }
}

/// One determinism run's MEASURED peak disk consumption on the worktree
/// volume.
///
/// `free_before` is the available space measured immediately before the
/// run's build started; `min_free_during` is the minimum the
/// [`FreeSpaceSampler`] observed while it ran. Their difference is the
/// peak the run needed at its worst moment — the figure the guard
/// projects forward to gate later runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunPeak {
    /// Available bytes measured before the run built.
    pub free_before: u64,
    /// Minimum available bytes observed during the run's build.
    pub min_free_during: u64,
}

impl RunPeak {
    /// Peak bytes the run consumed: the drop from `free_before` to the
    /// minimum observed mid-run.
    ///
    /// Clamped at `0`: a `min_free_during` somehow above `free_before`
    /// (free space grew mid-run — another process cleaned up) yields no
    /// measured pressure, so the floor becomes the only gate.
    pub fn consumed_bytes(&self) -> u64 {
        self.free_before.saturating_sub(self.min_free_during)
    }
}

/// Outcome of the per-run headroom check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadroomDecision {
    /// Enough free space: the run may proceed.
    Proceed,
    /// Insufficient free space. Carries the numbers for an actionable
    /// abort message ([`HeadroomShortfall::message`]).
    Abort(HeadroomShortfall),
}

/// The numbers behind an [`HeadroomDecision::Abort`], rendered into the
/// operator-facing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadroomShortfall {
    /// Zero-based index of the run that would have run next.
    pub run_idx: u32,
    /// Bytes the guard required to be free before the run.
    pub required_bytes: u64,
    /// Bytes actually free on the worktree volume.
    pub available_bytes: u64,
    /// The volume/path the readings were taken on (for the message).
    pub volume: String,
    /// Whether `required_bytes` came from a measured prior-run peak
    /// (`true`, run-1..N) or the absolute floor alone (`false`, run-0).
    /// Drives the message so it never over-claims a peak guarantee for
    /// run-0.
    pub peak_gated: bool,
}

impl HeadroomShortfall {
    /// Actionable, number-carrying abort message. Names the run, the
    /// deficit (with exact bytes so a near-miss is unambiguous), whether
    /// the bound was peak-measured or the floor backstop, and the two
    /// remedies an operator actually has.
    pub fn message(&self) -> String {
        let basis = if self.peak_gated {
            "the measured peak of a prior run's universal rebuild + dmg staging"
        } else {
            "the absolute floor for the first run's universal rebuild + dmg staging"
        };
        format!(
            "determinism run {} needs ~{} free ({}); only {} available on {}. \
             Strengthen reclaim-disk (raise the macOS shard's reclaim target) or use a \
             larger runner. Override the floor with {}=<GiB> / the safety factor with \
             {}=<f> if this estimate is wrong for your project.",
            self.run_idx + 1,
            format_gib_exact(self.required_bytes),
            basis,
            format_gib_exact(self.available_bytes),
            self.volume,
            ABS_FLOOR_ENV,
            SAFETY_FACTOR_ENV,
        )
    }
}

/// Project a measured peak forward into a required-free-space figure:
/// `peak × safety_factor`, saturating at `u64::MAX`.
///
/// Pulled out so the saturation intent is explicit (S1): an absurd peak ×
/// factor saturates to `u64::MAX`, which makes the guard always-abort —
/// the safe direction (never under-provision), and unreachable in
/// practice (a real peak is bounded by disk size).
fn project_peak(peak: u64, safety_factor: f64) -> u64 {
    let scaled = peak as f64 * safety_factor;
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

/// Decide whether a determinism run may proceed given the free space on
/// the worktree volume.
///
/// The required free space is the **larger** of:
/// - the absolute floor (`abs_floor`) — the sole gate before run-0 (when
///   `prior_peak` is `None`) and a backstop thereafter; and
/// - the largest prior-run MEASURED peak × `safety_factor` — the precise,
///   project-specific gate for run-1..N.
///
/// Taking the max means a project whose real peak exceeds the floor is
/// caught, while a project lighter than the floor is never rejected below
/// it. `peak_gated` in the returned shortfall reflects which term won, so
/// the message states the true guarantee.
///
/// Pure arithmetic over injected byte counts — no disk access — so the
/// threshold logic is testable without a real low-disk condition.
pub fn evaluate_headroom(
    run_idx: u32,
    available: u64,
    abs_floor: u64,
    prior_peak: Option<u64>,
    safety_factor: f64,
    volume: &str,
) -> HeadroomDecision {
    let projected = prior_peak.map(|p| project_peak(p, safety_factor));
    let peak_gated = projected.is_some_and(|p| p >= abs_floor);
    let required = abs_floor.max(projected.unwrap_or(0));
    if available >= required {
        HeadroomDecision::Proceed
    } else {
        HeadroomDecision::Abort(HeadroomShortfall {
            run_idx,
            required_bytes: required,
            available_bytes: available,
            volume: volume.to_string(),
            peak_gated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_source::MapEnvSource;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn format_gib_one_decimal() {
        assert_eq!(format_gib(0), "0.0 GiB");
        assert_eq!(format_gib(GIB), "1.0 GiB");
        assert_eq!(format_gib(GIB + GIB / 2), "1.5 GiB");
        assert_eq!(format_gib(101 * GIB + GIB / 5), "101.2 GiB");
    }

    #[test]
    fn format_gib_exact_carries_raw_bytes() {
        let s = format_gib_exact(12 * GIB);
        assert!(s.contains("12.00 GiB"), "two decimals: {s}");
        assert!(s.contains(&format!("{}", 12 * GIB)), "raw bytes: {s}");
    }

    #[test]
    fn available_bytes_probes_real_volume() {
        let dir = tempdir().unwrap();
        // A real filesystem always has *some* notion of available space;
        // the probe must return Some (we don't assert a floor — CI runners
        // vary — only that the call resolves).
        assert!(
            available_bytes(dir.path()).is_some(),
            "available_bytes must resolve on a real dir"
        );
    }

    #[test]
    fn available_bytes_missing_path_is_none_not_panic() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(available_bytes(&missing), None);
    }

    #[test]
    fn dir_size_sums_regular_files_recursively() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("b"), vec![0u8; 250]).unwrap();
        assert_eq!(dir_size_bytes(dir.path()), 350);
    }

    #[test]
    fn dir_size_missing_dir_is_zero() {
        let dir = tempdir().unwrap();
        assert_eq!(dir_size_bytes(&dir.path().join("nope")), 0);
    }

    #[test]
    fn dir_size_does_not_follow_symlinks() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("real"), vec![0u8; 500]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
            assert_eq!(dir_size_bytes(dir.path()), 500);
        }
    }

    #[test]
    fn sampler_records_a_minimum_on_a_real_volume() {
        // On a real dir the sampler must take at least one reading and
        // return Some — the value itself is runner-dependent.
        let dir = tempdir().unwrap();
        let s = FreeSpaceSampler::start(dir.path(), Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            s.stop().is_some(),
            "sampler must record a reading on a real volume"
        );
    }

    #[test]
    fn sampler_probe_gap_yields_none() {
        // A path that doesn't exist makes every probe fail; the sampler
        // records nothing and yields None (never a manufactured value).
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        let s = FreeSpaceSampler::start(&missing, Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(s.stop(), None);
    }

    #[test]
    fn run_peak_consumed_is_the_drop_to_the_minimum() {
        let p = RunPeak {
            free_before: 100 * GIB,
            min_free_during: 30 * GIB,
        };
        assert_eq!(p.consumed_bytes(), 70 * GIB);
    }

    #[test]
    fn run_peak_clamps_when_free_space_grew_mid_run() {
        let p = RunPeak {
            free_before: 80 * GIB,
            min_free_during: 90 * GIB,
        };
        assert_eq!(p.consumed_bytes(), 0);
    }

    #[test]
    fn headroom_run0_gated_by_absolute_floor_only_not_peak() {
        // No prior peak (run-0): the floor is the sole gate and the
        // shortfall must NOT claim a peak basis.
        let ok = evaluate_headroom(0, 50 * GIB, 45 * GIB, None, 1.3, "/");
        assert_eq!(ok, HeadroomDecision::Proceed);

        let bad = evaluate_headroom(0, 30 * GIB, 45 * GIB, None, 1.3, "/");
        match bad {
            HeadroomDecision::Abort(s) => {
                assert_eq!(s.run_idx, 0);
                assert_eq!(s.required_bytes, 45 * GIB);
                assert_eq!(s.available_bytes, 30 * GIB);
                assert!(!s.peak_gated, "run-0 is floor-gated, not peak-gated");
                assert!(
                    s.message().contains("absolute floor"),
                    "run-0 message must state the floor basis: {}",
                    s.message()
                );
            }
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn headroom_later_run_uses_measured_peak_when_above_floor() {
        // run-0 PEAKED at 70 GiB consumed; ×1.3 = 91 GiB required (above
        // the 45 GiB floor). This is the exact B1 failure: a net delta
        // would have seen ~30 GiB and wrongly proceeded.
        let prior_peak = Some(70 * GIB);
        let decision = evaluate_headroom(1, 71 * GIB, 45 * GIB, prior_peak, 1.3, "/Volumes/x");
        match decision {
            HeadroomDecision::Abort(s) => {
                assert_eq!(s.run_idx, 1);
                assert_eq!(s.required_bytes, 91 * GIB);
                assert_eq!(s.available_bytes, 71 * GIB);
                assert!(s.peak_gated, "run-1 must be peak-gated");
                assert!(
                    s.message().contains("measured peak"),
                    "message must state the peak basis: {}",
                    s.message()
                );
            }
            other => panic!("expected Abort (71 < 91), got {other:?}"),
        }
        // 95 GiB free clears the 91 GiB requirement.
        assert_eq!(
            evaluate_headroom(1, 95 * GIB, 45 * GIB, prior_peak, 1.3, "/Volumes/x"),
            HeadroomDecision::Proceed
        );
    }

    #[test]
    fn headroom_floor_backstops_a_tiny_measured_peak() {
        // A prior run that barely peaked; ×1.3 is below the floor, so the
        // floor still gates and the shortfall is NOT peak-gated.
        let prior_peak = Some(GIB);
        let bad = evaluate_headroom(2, 30 * GIB, 45 * GIB, prior_peak, 1.3, "/");
        match bad {
            HeadroomDecision::Abort(s) => {
                assert_eq!(s.required_bytes, 45 * GIB);
                assert!(!s.peak_gated, "floor won, so not peak-gated");
            }
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn project_peak_saturates_at_extremes_instead_of_wrapping() {
        // An absurd peak × factor saturates to u64::MAX (always-abort, the
        // safe direction) rather than wrapping to a small value.
        assert_eq!(project_peak(u64::MAX, 2.0), u64::MAX);
        assert_eq!(
            project_peak(10 * GIB, 1.3),
            (10.0 * GIB as f64 * 1.3) as u64
        );
    }

    #[test]
    fn shortfall_message_carries_exact_numbers_and_remedies() {
        let s = HeadroomShortfall {
            run_idx: 1,
            required_bytes: 91 * GIB,
            available_bytes: 71 * GIB,
            volume: "/Volumes/scratch".into(),
            peak_gated: true,
        };
        let msg = s.message();
        assert!(msg.contains("determinism run 2"), "1-based run: {msg}");
        assert!(
            msg.contains(&format!("{}", 91 * GIB)),
            "exact required: {msg}"
        );
        assert!(
            msg.contains(&format!("{}", 71 * GIB)),
            "exact available: {msg}"
        );
        assert!(msg.contains("/Volumes/scratch"), "volume: {msg}");
        assert!(msg.contains("reclaim-disk"), "remedy: {msg}");
        assert!(msg.contains(ABS_FLOOR_ENV), "floor override hint: {msg}");
        assert!(
            msg.contains(SAFETY_FACTOR_ENV),
            "factor override hint: {msg}"
        );
    }

    #[test]
    fn abs_floor_env_rejects_zero_malformed_overflow_accepts_valid() {
        // Injected env via MapEnvSource — no process-env mutation, so the
        // test is race-free and needs neither `unsafe` nor `#[serial]`.
        let cases: &[(&str, u64)] = &[
            ("0", DEFAULT_ABS_FLOOR_BYTES),           // B2: 0 must not disable
            ("abc", DEFAULT_ABS_FLOOR_BYTES),         // malformed
            ("18446744073", DEFAULT_ABS_FLOOR_BYTES), // B3: × GiB overflows u64
            ("60", 60 * GIB),                         // valid
            ("", DEFAULT_ABS_FLOOR_BYTES),            // empty
        ];
        for (val, want) in cases {
            let env = MapEnvSource::new().with(ABS_FLOOR_ENV, *val);
            assert_eq!(
                abs_floor_bytes_from_env_with(&env),
                *want,
                "ABS_FLOOR_ENV={val:?} should resolve to {want}"
            );
        }
        // Missing key (var simply absent from the map) → default.
        assert_eq!(
            abs_floor_bytes_from_env_with(&MapEnvSource::new()),
            DEFAULT_ABS_FLOOR_BYTES,
            "unset → default"
        );
    }

    #[test]
    fn safety_factor_env_rejects_nonpositive_accepts_valid() {
        let cases: &[(&str, f64)] = &[
            ("0", DEFAULT_SAFETY_FACTOR),
            ("-1.0", DEFAULT_SAFETY_FACTOR),
            ("nan", DEFAULT_SAFETY_FACTOR),
            ("xyz", DEFAULT_SAFETY_FACTOR),
            ("2.0", 2.0),
        ];
        for (val, want) in cases {
            let env = MapEnvSource::new().with(SAFETY_FACTOR_ENV, *val);
            assert!(
                (safety_factor_from_env_with(&env) - want).abs() < f64::EPSILON,
                "SAFETY_FACTOR_ENV={val:?} should resolve to {want}"
            );
        }
        // Missing key → default.
        assert!(
            (safety_factor_from_env_with(&MapEnvSource::new()) - DEFAULT_SAFETY_FACTOR).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn defaults_are_the_documented_values() {
        // A minimal liveness floor — NOT a workload peak. The measured-peak
        // gate (run-1+) is the heavy-workload protection.
        assert_eq!(DEFAULT_ABS_FLOOR_BYTES, 2 * GIB);
        assert!((DEFAULT_SAFETY_FACTOR - 1.3).abs() < f64::EPSILON);
    }
}
