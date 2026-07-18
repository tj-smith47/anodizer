//! Uniform retry-with-exponential-backoff primitives.
//!
//! Replaces six open-coded retry loops in `stage-docker` (3×) and
//! `stage-release` (3×) that had diverged on backoff formulas —
//! `2^(n-2)`, `2^(n-1)`, and `500 << (attempt-1)` all coexisted.
//!
//! The canonical policy is exponential backoff with multiplier 2 starting at
//! `base_delay` and capped at `max_delay`:
//!
//! ```text
//! attempt 1:  f() executes immediately
//! attempt 2:  sleep base_delay
//! attempt 3:  sleep base_delay * 2
//! attempt N:  sleep min(base_delay * 2^(N-2), max_delay)
//! ```
//!
//! `ControlFlow<Break, Continue>` lets the operation decide retry policy per
//! failure (e.g. 4xx → Break, 5xx → Continue) without the helper encoding
//! protocol-specific predicates.
//!
//! Both a sync (`retry_sync`) and async (`retry_async`) variant are provided so
//! that sites can adopt without crossing a sync/async boundary.

mod driver;
mod error;
mod http;
mod policy;

pub use driver::*;
pub use error::*;
pub use http::*;
pub use policy::*;

#[cfg(test)]
mod tests;
