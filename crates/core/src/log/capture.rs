//! In-memory log capture for tests (`test-helpers` feature).

// Every item in this module is `test-helpers`-gated, so the imports must carry
// the same gate or they are unused in a default build.
#[cfg(feature = "test-helpers")]
use std::sync::Arc;
#[cfg(feature = "test-helpers")]
use std::sync::Mutex;

/// Level of a log line captured by a [`LogCapture`]. Mirrors the
/// [`StageLogger`] methods that produce each level.
///
/// Gated behind the `test-helpers` Cargo feature — production binaries
/// do not link the capture infrastructure.
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Status,
    /// Liveness / progress line from [`StageLogger::heartbeat`], surfaced
    /// during a slow subprocess or upload. A level of its own so a heartbeat is
    /// never counted as a status result line.
    Heartbeat,
    Verbose,
    Debug,
}

/// In-memory sink that records every log line a [`StageLogger`] emits.
///
/// Cheap clone (`Arc<Mutex<Vec<…>>>` underneath) — pass the same handle to
/// every logger derived from a [`crate::context::Context`] and read aggregated
/// counts back via the accessor methods. Intended for tests that need to
/// assert "publisher emitted ≥N status lines" — calls still write to stderr
/// so test output stays debuggable.
///
/// Gated behind the `test-helpers` Cargo feature.
#[cfg(feature = "test-helpers")]
#[derive(Clone, Default)]
pub struct LogCapture {
    inner: Arc<Mutex<Vec<(LogLevel, String)>>>,
}

#[cfg(feature = "test-helpers")]
impl LogCapture {
    /// Construct a fresh empty capture sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a log line to the capture vec. Called from the
    /// [`StageLogger`] methods when a capture is attached.
    pub(crate) fn record(&self, level: LogLevel, msg: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.push((level, msg.into()));
        }
    }

    /// Number of [`LogLevel::Status`] lines recorded.
    pub fn status_count(&self) -> usize {
        self.count(LogLevel::Status)
    }

    /// Number of [`LogLevel::Debug`] lines recorded.
    pub fn debug_count(&self) -> usize {
        self.count(LogLevel::Debug)
    }

    /// Number of [`LogLevel::Verbose`] lines recorded.
    pub fn verbose_count(&self) -> usize {
        self.count(LogLevel::Verbose)
    }

    /// Number of [`LogLevel::Warn`] lines recorded.
    pub fn warn_count(&self) -> usize {
        self.count(LogLevel::Warn)
    }

    /// Number of [`LogLevel::Heartbeat`] lines recorded.
    pub fn heartbeat_count(&self) -> usize {
        self.count(LogLevel::Heartbeat)
    }

    /// Number of [`LogLevel::Error`] lines recorded.
    pub fn error_count(&self) -> usize {
        self.count(LogLevel::Error)
    }

    /// Total count across all levels (useful sanity check).
    pub fn total_count(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    fn count(&self, level: LogLevel) -> usize {
        self.inner
            .lock()
            .map(|g| g.iter().filter(|(l, _)| *l == level).count())
            .unwrap_or(0)
    }

    /// Snapshot of every recorded line in insertion order.
    pub fn all_messages(&self) -> Vec<(LogLevel, String)> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Snapshot of every [`LogLevel::Warn`] message in insertion order.
    ///
    /// Convenience accessor for tests that care only about warns — strips
    /// the level tuple [`all_messages`] returns so callers can write
    /// `cap.warn_messages().iter().any(|m| m.contains("..."))` without
    /// the per-call filter+map boilerplate.
    ///
    /// [`all_messages`]: Self::all_messages
    pub fn warn_messages(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|(lvl, _)| *lvl == LogLevel::Warn)
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}
