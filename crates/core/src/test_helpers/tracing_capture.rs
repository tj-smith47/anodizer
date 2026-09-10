//! One capturing `tracing` subscriber for the tests that assert on warnings.
//!
//! Three verbatim copies of this block lived in one crate, and they were not
//! independent: `with_default` installs a subscriber thread-locally, but the
//! max-level hint and the callsite `Interest` cache it primes are process-wide,
//! so two copies running concurrently made each other's events vanish. One
//! helper plus the serial key below is the mechanical form of that remedy.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

/// Shared buffer every `make_writer` clone appends to.
#[derive(Clone, Default)]
pub struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl BufferWriter {
    /// Everything written so far, lossily decoded.
    pub fn captured(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).to_string()
    }
}

impl io::Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
    type Writer = BufferWriterGuard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        BufferWriterGuard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The lock a single `make_writer` call holds, so one event is one append.
pub struct BufferWriterGuard<'a>(MutexGuard<'a, Vec<u8>>);

impl io::Write for BufferWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run `body` under a `WARN`-level capturing subscriber and return everything
/// it emitted, without timestamps or ANSI so assertions can match plain text.
///
/// The subscriber is thread-local, but the level hint it primes is not: every
/// test calling this must carry `#[serial_test::serial(tracing)]`, which
/// `every_tracing_capture_test_takes_the_serial_key` enforces.
pub fn capture_tracing_warnings<F: FnOnce()>(body: F) -> String {
    let buf = BufferWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    buf.captured()
}

#[cfg(test)]
mod tests {
    use super::super::test_sources;
    use std::path::{Path, PathBuf};

    /// Every `.rs` file under the crate's `src/`, production and test halves
    /// alike: an inline `#[cfg(test)]` module lives in a production file, so
    /// asking only for the test half would miss most of the call sites.
    fn every_source() -> Vec<PathBuf> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut all = test_sources::rust_sources(&src);
        all.extend(test_sources::test_sources(&src));
        // This file's own tests carry the scanner's fixture as a string
        // literal, which the line scan cannot tell from a call site.
        all.retain(|p| !p.ends_with("tracing_capture.rs"));
        all
    }

    /// Names of the functions in `src` that call the capture helper, paired
    /// with whether their attribute run carries the serial key.
    fn capture_call_sites(src: &str) -> Vec<(String, bool)> {
        let lines: Vec<&str> = src.lines().collect();
        let mut sites = Vec::new();
        let mut current: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("pub(super) fn ")
            {
                current = Some(i);
            }
            if !line.contains("capture_tracing_warnings(") || trimmed.starts_with("///") {
                continue;
            }
            let Some(fn_line) = current else { continue };
            let name = lines[fn_line]
                .trim_start()
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub(super) ")
                .trim_start_matches("pub ")
                .trim_start_matches("fn ")
                .split('(')
                .next()
                .unwrap_or_default()
                .to_string();
            if name == "capture_tracing_warnings" {
                continue;
            }
            // The attribute run above the signature: attributes, doc comments
            // and plain comments, ending at the first line that is none.
            let mut attrs = String::new();
            for prev in lines[..fn_line].iter().rev() {
                let p = prev.trim_start();
                if p.starts_with('#') || p.starts_with("//") {
                    attrs.push_str(p);
                    attrs.push('\n');
                } else {
                    break;
                }
            }
            if !attrs.contains("#[test]") {
                continue;
            }
            sites.push((name, attrs.contains("serial(tracing)")));
        }
        sites.sort();
        sites.dedup();
        sites
    }

    #[test]
    fn every_tracing_capture_test_takes_the_serial_key() {
        let mut unmarked = Vec::new();
        let mut total = 0usize;
        for path in every_source() {
            let src = std::fs::read_to_string(&path).expect("read source");
            for (name, serial) in capture_call_sites(&src) {
                total += 1;
                if !serial {
                    unmarked.push(format!("{}::{name}", path.display()));
                }
            }
        }
        assert!(
            total >= 8,
            "the walk found only {total} capture tests; it stopped seeing the population"
        );
        assert!(
            unmarked.is_empty(),
            "a tracing subscriber is installed thread-locally but primes a \
             process-global level hint, so every capture test needs \
             #[serial_test::serial(tracing)]; missing on {unmarked:?}"
        );
    }

    #[test]
    fn a_capture_test_without_the_serial_key_is_reported() {
        let src = "\
#[test]
fn marked() {
    let out = capture_tracing_warnings(|| {});
}

#[test]
fn unmarked() {
    let out = capture_tracing_warnings(|| {});
}
";
        let src = src.replace(
            "#[test]\nfn marked()",
            "#[test]\n#[serial_test::serial(tracing)]\nfn marked()",
        );
        assert_eq!(
            capture_call_sites(&src),
            vec![
                ("marked".to_string(), true),
                ("unmarked".to_string(), false)
            ]
        );
    }
}
