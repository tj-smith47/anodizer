//! Drift localization + injection helpers.
//!
//! - [`summarize_drift`] turns per-run sample data into a one-line
//!   "first diff at offset N" string so operators can target the
//!   region without an external hex-dump diff round-trip.
//! - [`pick_first_artifact_for_stage`] + [`inject_drift_byte`] back
//!   the `--inject-drift=<stage>` test-harness flag.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::artifacts::{ArtifactInfo, infer_stage_from_path};

/// Produce a one-line human-readable summary of where two runs' head
/// samples diverge for a given artifact. Returns `None` when fewer
/// than two runs have head-sample data for the artifact (the comparison
/// is meaningless with one data point — the surrounding `if all_equal`
/// already established that the hashes differed, so the missing-sample
/// path is purely a defensive guard, not the common case).
///
/// Output shapes (first applicable):
///   - `"first diff at offset 0xNN (run0=0xXX, run1=0xYY)"` — head
///     diverges within `HEAD_SAMPLE_BYTES`.
///   - `"tail diff at -0xNN from end (size B, run0=0xXX, run1=0xYY)"` —
///     heads match but tails diverge; `-0xNN` is the offset from EOF.
///     Catches gzip/zip footer drift, signature trailers, PE Debug
///     Directory drift.
///   - `"no diff in first/last K bytes; sizes differ..."` — both
///     sampled windows match but total sizes differ: drift is in the
///     un-sampled middle.
///
/// For three-or-more-run reports (currently the harness defaults to 2
/// but `--runs=N` is a CLI flag), the summary compares run0 vs the
/// first differing run; if all subsequent runs also diverge from run0,
/// reporting the first divergence is sufficient to localize the source.
pub(super) fn summarize_drift(
    name: &str,
    per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
) -> Option<String> {
    let samples: Vec<(&[u8], &[u8], u64)> = per_run_hashes
        .iter()
        .filter_map(|run| {
            run.get(name).map(|info| {
                (
                    info.head_sample.as_slice(),
                    info.tail_sample.as_slice(),
                    info.size_bytes,
                )
            })
        })
        .collect();
    if samples.len() < 2 {
        return None;
    }
    let (head0, tail0, size0) = samples[0];
    if let Some((idx, head_n, offset)) =
        samples
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(idx, &(head_n, _, _))| {
                let common = head0.len().min(head_n.len());
                (0..common)
                    .find(|&i| head0[i] != head_n[i])
                    .map(|off| (idx, head_n, off))
            })
    {
        return Some(format!(
            "first diff at offset {:#x} (run0={:#04x}, run{idx}={:#04x})",
            offset, head0[offset], head_n[offset]
        ));
    }
    if let Some((idx, head_n)) = samples
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(idx, &(head_n, _, _))| (head_n.len() != head0.len()).then_some((idx, head_n)))
    {
        return Some(format!(
            "head samples differ in length: run0={} bytes, run{idx}={} bytes",
            head0.len(),
            head_n.len()
        ));
    }
    // Heads match. Check the tail window for trailing-section drift.
    // Only meaningful when both runs captured a non-empty tail and
    // total sizes agree (otherwise the size-diff branch below is more
    // informative — different EOFs make tail offsets compare apples
    // to oranges).
    if !tail0.is_empty()
        && let Some((idx, tail_n, offset)) =
            samples
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(idx, &(_, tail_n, size_n))| {
                    if tail_n.is_empty() || size_n != size0 || tail_n.len() != tail0.len() {
                        return None;
                    }
                    (0..tail0.len())
                        .find(|&i| tail0[i] != tail_n[i])
                        .map(|off| (idx, tail_n, off))
                })
    {
        let from_end = tail0.len() - offset;
        return Some(format!(
            "tail diff at -{:#x} from end (size {}, run0={:#04x}, run{idx}={:#04x})",
            from_end, size0, tail0[offset], tail_n[offset]
        ));
    }
    if let Some((idx, size_n)) = samples
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(idx, &(_, _, size_n))| (size_n != size0).then_some((idx, size_n)))
    {
        return Some(format!(
            "no diff in first {} or last {} bytes; total size run0={} run{idx}={} \
             (drift in un-sampled middle)",
            head0.len(),
            tail0.len(),
            size0,
            size_n
        ));
    }
    Some(format!(
        "no diff in first {} or last {} bytes; sizes equal at {} bytes \
         (drift in un-sampled middle)",
        head0.len(),
        tail0.len(),
        size0
    ))
}

/// Pick the first artifact whose inferred stage matches `stage_name`,
/// in the sorted order returned by `discover_artifacts`. Returns
/// `None` when no artifact maps to the named stage (caller silently
/// no-ops — the integration test should observe drift_count == 0 in
/// that case, surfacing a typo in the stage value).
pub(super) fn pick_first_artifact_for_stage<'a>(
    artifacts: &'a [PathBuf],
    stage_name: &str,
) -> Option<&'a PathBuf> {
    artifacts.iter().find(|p| {
        let rel = p.to_string_lossy();
        infer_stage_from_path(&rel) == stage_name
    })
}

/// Append one byte to `path` to force the artifact to differ across
/// runs. Used by the `--inject-drift=<stage>` test-harness flag.
///
/// Source byte: `/dev/urandom` on platforms that expose it;
/// `SystemTime::now().subsec_nanos()` fallback otherwise. The fallback
/// MUST vary between successive runs — when the underlying archive is
/// fully deterministic (the goal of this harness), appending a
/// CONSTANT byte to two byte-identical archives yields two
/// byte-identical archives and the harness reports no drift. The
/// nanos fallback varies on every call (successive harness runs are
/// at least milliseconds apart), so the appended byte differs across
/// runs and the hash diverges as intended.
pub(super) fn inject_drift_byte(path: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let byte: u8 = match std::fs::OpenOptions::new().read(true).open("/dev/urandom") {
        Ok(mut f) => {
            let mut buf = [0u8; 1];
            f.read_exact(&mut buf).ok();
            buf[0]
        }
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u8)
            .unwrap_or(0xAB),
    };
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    f.write_all(&[byte])
        .with_context(|| format!("appending drift byte to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_first_artifact_for_stage_picks_first_by_inferred_stage() {
        let artifacts = vec![
            PathBuf::from("dist/checksums.txt"),
            PathBuf::from("dist/foo.tar.gz"),
            PathBuf::from("dist/bar.tar.gz"),
        ];
        let pick = pick_first_artifact_for_stage(&artifacts, "archive").unwrap();
        assert_eq!(pick, &PathBuf::from("dist/foo.tar.gz"));
        let pick = pick_first_artifact_for_stage(&artifacts, "checksum").unwrap();
        assert_eq!(pick, &PathBuf::from("dist/checksums.txt"));
    }

    #[test]
    fn pick_first_artifact_for_stage_returns_none_for_missing_stage() {
        let artifacts = vec![PathBuf::from("dist/foo.tar.gz")];
        assert!(pick_first_artifact_for_stage(&artifacts, "sbom").is_none());
        assert!(pick_first_artifact_for_stage(&artifacts, "bogus-stage").is_none());
    }

    #[test]
    fn inject_drift_byte_mutates_file_so_hash_differs() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("victim.bin");
        std::fs::write(&p, b"hello world").unwrap();
        let before = {
            let mut h = Sha256::new();
            h.update(std::fs::read(&p).unwrap());
            format!("{:x}", h.finalize())
        };
        inject_drift_byte(&p).expect("inject");
        let after_bytes = std::fs::read(&p).unwrap();
        let after = {
            let mut h = Sha256::new();
            h.update(&after_bytes);
            format!("{:x}", h.finalize())
        };
        assert_ne!(before, after, "hash must change after drift injection");
        assert_eq!(
            after_bytes.len(),
            b"hello world".len() + 1,
            "exactly one byte must be appended"
        );
    }
}
