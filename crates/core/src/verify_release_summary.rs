//! Post-publish verification outcome, threaded from the verify-release stage
//! into the end-of-pipeline run summary.
//!
//! The verify-release gate runs LAST and `bail!`s when it finds defects, so by
//! the time the summary is built the publish report still reads all-`succeeded`
//! (the publishes genuinely landed). This slot lets the summary state the
//! SEPARATE verify-release verdict, so a failing post-publish check surfaces in
//! the final status block rather than being buried in a stderr `bail!` — the
//! publishes are real, but the release has unverified defects the operator must
//! investigate.

/// The verify-release stage's verdict for one pipeline run.
///
/// "Did the gate run" is encoded by `Some(..)` vs `None` on the
/// [`Context`](crate::context::Context) slot: the stage stamps `Some` whenever
/// it executed its checks, and leaves the slot `None` on the disabled /
/// skipped / dry-run / snapshot early-returns, where no published release
/// exists to verify. `issues` is empty on a clean pass and carries one
/// human-readable string per defect otherwise; each string already names the
/// offending `crate '<name>'` so attribution survives the workspace fan-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReleaseSummary {
    /// One message per detected defect; empty on a clean pass.
    pub issues: Vec<String>,
}
