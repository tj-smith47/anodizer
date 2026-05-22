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
/// Output shapes (first applicable, all using **absolute byte offsets**
/// in the file so the operator can `dd skip=N` directly without
/// translating from-end coordinates):
///   - `"first diff at offset 0xNN (run0=0xXX, run1=0xYY)"` — head
///     diverges within `HEAD_SAMPLE_BYTES`.
///   - `"tail diff at offset 0xNN (size B, run0=0xXX, run1=0xYY)"` —
///     heads match but tails diverge. Catches gzip/zip footer drift,
///     signature trailers, PE Debug Directory drift, and the
///     mid-tail gap for files in `(HEAD, HEAD+TAIL]`.
///   - `"no diff in first/last K bytes; sizes differ..."` — both
///     sampled windows match but total sizes differ: drift is in the
///     un-sampled middle.
///
/// When the artifact is small enough that head + tail cover every byte
/// (artifact size ≤ `HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES`) AND the
/// content is plausibly text (no NUL bytes in the head sample, JSON /
/// YAML / TOML / TXT extension), the byte-offset line is followed by a
/// `\n`-separated `text drift detected: ...` block showing the
/// containing line in each run. This is the difference between the
/// operator seeing `"first diff at offset 0x3461"` and seeing
/// `run0 line 142: "size": 119,` / `run1 line 142: "size": 120,` —
/// the latter localizes the *field* without a download round-trip.
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
        let base = format!(
            "first diff at offset {:#x} (run0={:#04x}, run{idx}={:#04x})",
            offset, head0[offset], head_n[offset]
        );
        return Some(append_text_diff(base, name, &samples, idx, offset));
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
        && let Some((idx, tail_n, size_n, off_in_tail)) = samples
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(idx, &(_, tail_n, size_n))| {
                if tail_n.is_empty() || size_n != size0 || tail_n.len() != tail0.len() {
                    return None;
                }
                (0..tail0.len())
                    .find(|&i| tail0[i] != tail_n[i])
                    .map(|off| (idx, tail_n, size_n, off))
            })
    {
        // Absolute file offset = tail_start + off_in_tail.
        // tail_start = size - tail.len() — works for both the
        // end-of-file tail (size > HEAD+TAIL) and the
        // gap-closing tail (HEAD < size ≤ HEAD+TAIL where
        // tail_start = HEAD_SAMPLE_BYTES).
        let _ = size_n; // kept above for guard, unused in summary
        let tail_start = (size0 as usize).saturating_sub(tail0.len());
        let abs_offset = tail_start + off_in_tail;
        let base = format!(
            "tail diff at offset {:#x} (size {}, run0={:#04x}, run{idx}={:#04x})",
            abs_offset, size0, tail0[off_in_tail], tail_n[off_in_tail]
        );
        return Some(append_text_diff(base, name, &samples, idx, abs_offset));
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

/// Reconstruct full artifact bytes from a head + tail sample pair when
/// the two together cover every byte of the file (no un-sampled
/// middle). Used by the text-diff enrichment so [`summarize_drift`]
/// can show the actual differing JSON / TOML / YAML line rather than
/// only an offset.
///
/// Returns `None` when:
///   - `head.len() + tail.len() < size` (real coverage gap — drift
///     might land in the un-sampled middle and we can't decode it),
///   - `size` exceeds `usize::MAX` on the host (a 32-bit corner case
///     for hypothetical >4 GiB artifacts; the harness's hashing path
///     already buffers the file fully, so 32-bit hosts couldn't get
///     here either way).
///
/// Guaranteed invariant when `Some(_)`: returned buffer length equals
/// `size`. Caller can compare two reconstructions byte-for-byte.
fn reconstruct_full(head: &[u8], tail: &[u8], size: u64) -> Option<Vec<u8>> {
    let size = usize::try_from(size).ok()?;
    if head.len() + tail.len() < size {
        return None;
    }
    // tail_start = size - tail.len(), guaranteed >= head's covered
    // region by the sampler's construction. If the sampler ever
    // emits an unaligned pair (regression) the strict bounds check
    // here guards us.
    let tail_start = size.checked_sub(tail.len())?;
    if tail_start > head.len() {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&head[..tail_start]);
    out.extend_from_slice(tail);
    Some(out)
}

/// Heuristic: does this artifact look textual? Conservative — false
/// negatives are fine (text-diff just doesn't fire), false positives
/// produce ugly mojibake in the report.
///
/// Rules:
///   1. Extension whitelist: `.json`, `.txt`, `.yaml`, `.yml`,
///      `.toml`, `.csv`, `.md`. These are the formats the pipeline
///      emits as readable text (manifests, checksums, SBOMs).
///   2. Plus: the head sample must contain no NUL byte. A binary
///      that happens to be named `foo.txt` (uncommon, but possible
///      for snapshot tests) still gets a clean byte-offset summary.
fn looks_textual(name: &str, head: &[u8]) -> bool {
    let lower = name.to_lowercase();
    let ext_ok = lower.ends_with(".json")
        || lower.ends_with(".txt")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".toml")
        || lower.ends_with(".csv")
        || lower.ends_with(".md");
    ext_ok && !head.contains(&0)
}

/// Locate the line containing `abs_offset` in `bytes`. Returns the
/// 1-indexed line number plus the line content (UTF-8 lossy, trailing
/// `\n` excluded). When the offset lands past EOF (e.g. one run is
/// shorter), returns the last line.
fn line_at_offset(bytes: &[u8], abs_offset: usize) -> (usize, String) {
    let clamped = abs_offset.min(bytes.len().saturating_sub(1));
    let prefix = &bytes[..=clamped];
    let line_start = prefix
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_end = bytes[line_start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| line_start + p)
        .unwrap_or(bytes.len());
    let line_no = bytes[..line_start].iter().filter(|&&b| b == b'\n').count() + 1;
    let s = String::from_utf8_lossy(&bytes[line_start..line_end]).into_owned();
    (line_no, s)
}

/// Append a `text drift detected: ...` block to `base` when the
/// artifact is small enough to fully reconstruct AND looks textual.
/// On any precondition miss returns `base` unchanged so the operator
/// still gets the byte-offset summary.
fn append_text_diff(
    base: String,
    name: &str,
    samples: &[(&[u8], &[u8], u64)],
    idx: usize,
    abs_offset: usize,
) -> String {
    let (head0, tail0, size0) = samples[0];
    let (head_n, tail_n, size_n) = samples[idx];
    if !looks_textual(name, head0) {
        return base;
    }
    let Some(bytes0) = reconstruct_full(head0, tail0, size0) else {
        return base;
    };
    let Some(bytes_n) = reconstruct_full(head_n, tail_n, size_n) else {
        return base;
    };
    let (ln0, line0) = line_at_offset(&bytes0, abs_offset);
    let (ln_n, line_n) = line_at_offset(&bytes_n, abs_offset);
    // Truncate at a generous 240 chars so a long minified JSON line
    // still leaves the offset summary visible. The CI artifact tarball
    // still contains the raw files for full inspection.
    let line0 = truncate_for_summary(&line0, 240);
    let line_n = truncate_for_summary(&line_n, 240);
    format!(
        "{base}\ntext drift detected:\n  run0 line {ln0}: {line0}\n  run{idx} line {ln_n}: {line_n}"
    )
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
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
/// Byte source: a process-local atomic counter, incremented per call.
/// MUST vary between successive runs — when the underlying archive is
/// fully deterministic (the goal of this harness), appending a CONSTANT
/// byte to two byte-identical archives yields two byte-identical
/// archives and the harness reports no drift.
///
/// The earlier `/dev/urandom`-or-`subsec_nanos()` source was flaky on
/// Windows runners: no `/dev/urandom`, and consecutive runs can land
/// in the same nanos-mod-256 window (100 ns clock resolution × u8
/// truncation), producing identical injected bytes. The counter is
/// monotonic per-process and platform-uniform, eliminating the flake.
pub(super) fn inject_drift_byte(path: &Path) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU8, Ordering};
    static DRIFT_BYTE_COUNTER: AtomicU8 = AtomicU8::new(1);
    let byte = DRIFT_BYTE_COUNTER.fetch_add(1, Ordering::Relaxed);
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

    /// Construct a synthetic two-run sample map for `summarize_drift`
    /// using full byte content per run. The helper picks head/tail
    /// samples with the same rules as `hash_artifacts` so the
    /// summarizer sees the same shape it would in production.
    fn samples_from_bytes(
        name: &str,
        bytes_per_run: &[&[u8]],
    ) -> Vec<BTreeMap<String, super::ArtifactInfo>> {
        use super::super::artifacts::{HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES};
        bytes_per_run
            .iter()
            .map(|bytes| {
                let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
                let head_sample = bytes[..head_len].to_vec();
                let tail_sample: Vec<u8> = if bytes.len() <= HEAD_SAMPLE_BYTES {
                    Vec::new()
                } else {
                    let tail_start = bytes
                        .len()
                        .saturating_sub(TAIL_SAMPLE_BYTES)
                        .max(HEAD_SAMPLE_BYTES);
                    bytes[tail_start..].to_vec()
                };
                let mut map = BTreeMap::new();
                map.insert(
                    name.to_string(),
                    super::ArtifactInfo {
                        hash: "sha256:fixture".into(),
                        size_bytes: bytes.len() as u64,
                        relative_path: format!("dist/{name}"),
                        stage: "unknown".into(),
                        head_sample,
                        tail_sample,
                    },
                );
                map
            })
            .collect()
    }

    /// Sampler regression guard: an artifact in the (HEAD, HEAD+TAIL]
    /// size band MUST yield a non-empty tail_sample that — together
    /// with the head — covers every byte. The prior shape returned an
    /// empty tail in that band, producing the "drift in un-sampled
    /// middle" black hole that masked v0.3.0's artifacts.json drift.
    #[test]
    fn samples_from_bytes_mid_size_files_have_no_unsampled_gap() {
        use super::super::artifacts::{HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES};
        // 24 KiB — between HEAD (16 KiB) and HEAD+TAIL (32 KiB).
        let mut payload = vec![0u8; 24 * 1024];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (i & 0xff) as u8;
        }
        let samples = samples_from_bytes("artifacts.json", &[&payload]);
        let info = samples[0].get("artifacts.json").unwrap();
        assert_eq!(info.head_sample.len(), HEAD_SAMPLE_BYTES);
        assert!(
            !info.tail_sample.is_empty(),
            "tail must be non-empty for mid-size files; got {} bytes",
            info.tail_sample.len()
        );
        // Coverage: head ends at HEAD_SAMPLE_BYTES, tail starts at HEAD,
        // tail ends at file size — no gap.
        let tail_start = payload.len() - info.tail_sample.len();
        assert_eq!(
            tail_start, HEAD_SAMPLE_BYTES,
            "tail must start exactly where head ends to close the gap"
        );
        let _ = TAIL_SAMPLE_BYTES; // kept imported for sibling tests
    }

    /// `summarize_drift` MUST report a real byte offset (not `"drift in
    /// un-sampled middle"`) when the artifact size fits inside
    /// HEAD+TAIL. Closes the v0.3.0 regression where late-mid-file
    /// drift was unlocalized.
    #[test]
    fn summarize_drift_localizes_mid_size_file_drift() {
        // 20 KiB, differ at byte 18000 (well past HEAD_SAMPLE_BYTES).
        let mut run0 = vec![0xaau8; 20 * 1024];
        let mut run1 = run0.clone();
        run0[18_000] = 0x11;
        run1[18_000] = 0x22;
        let samples = samples_from_bytes("artifacts.json", &[&run0, &run1]);
        let summary = summarize_drift("artifacts.json", &samples).unwrap();
        assert!(
            summary.contains("offset 0x4650"),
            "expected absolute offset 0x4650 (=18000), got: {summary}"
        );
        // Bytes at that offset must be surfaced.
        assert!(summary.contains("0x11"), "got: {summary}");
        assert!(summary.contains("0x22"), "got: {summary}");
    }

    /// Text-diff enrichment: when the artifact looks textual (JSON
    /// extension, no NULs) AND head + tail cover the file, the summary
    /// MUST include the differing line in each run so the operator
    /// sees the field name, not just a byte offset.
    #[test]
    fn summarize_drift_emits_text_diff_for_json_artifacts() {
        let run0 = br#"{
  "name": "anodize",
  "size": 119,
  "kind": "Signature"
}"#;
        let run1 = br#"{
  "name": "anodize",
  "size": 120,
  "kind": "Signature"
}"#;
        let samples = samples_from_bytes("artifacts.json", &[run0.as_slice(), run1.as_slice()]);
        let summary = summarize_drift("artifacts.json", &samples).unwrap();
        assert!(
            summary.contains("text drift detected"),
            "expected text-diff block; got: {summary}"
        );
        assert!(
            summary.contains("\"size\": 119"),
            "expected run0 line content; got: {summary}"
        );
        assert!(
            summary.contains("\"size\": 120"),
            "expected run1 line content; got: {summary}"
        );
    }

    /// Text-diff MUST NOT fire on binary artifacts even when fully
    /// reconstructable. Binary mojibake in the report makes it harder
    /// to read, not easier.
    #[test]
    fn summarize_drift_skips_text_diff_for_binary_artifacts() {
        let mut run0 = vec![0u8; 256];
        let mut run1 = run0.clone();
        run0[100] = 0x11;
        run1[100] = 0x22;
        let samples = samples_from_bytes("anodize.exe", &[&run0, &run1]);
        let summary = summarize_drift("anodize.exe", &samples).unwrap();
        assert!(
            !summary.contains("text drift detected"),
            "binary artifact must not trigger text diff; got: {summary}"
        );
        assert!(
            summary.contains("offset 0x64"),
            "byte-offset summary still required; got: {summary}"
        );
    }

    /// Tail diff uses absolute byte offset (the format change from
    /// `"tail diff at -0xN from end"` to `"tail diff at offset 0xN"`).
    /// Operator can `dd skip=N` directly without translating
    /// from-end coordinates.
    #[test]
    fn summarize_drift_tail_diff_reports_absolute_offset() {
        use super::super::artifacts::{HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES};
        // 40 KiB — large enough that head/tail are non-overlapping and
        // there IS an un-sampled middle. Drift at the very last byte
        // lands in the tail.
        let size = HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES + 8 * 1024;
        let mut run0 = vec![0x55u8; size];
        let mut run1 = run0.clone();
        run0[size - 1] = 0xaa;
        run1[size - 1] = 0xbb;
        let samples = samples_from_bytes("big.bin", &[&run0, &run1]);
        let summary = summarize_drift("big.bin", &samples).unwrap();
        let expected_offset = format!("{:#x}", size - 1);
        assert!(
            summary.contains(&expected_offset),
            "expected absolute offset {expected_offset}; got: {summary}"
        );
        assert!(
            !summary.contains("from end"),
            "tail summary must no longer use from-end coordinates; got: {summary}"
        );
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
