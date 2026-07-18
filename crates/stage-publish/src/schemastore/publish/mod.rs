//! SchemaStore publish orchestration: turns the configured `schemas` into a
//! single pull request against a fork of `SchemaStore/schemastore`, reusing
//! krew's clone/commit/push/PR machinery and delegating every decision to the
//! pure helpers in `catalog`/`manifest`.
//!
//! The decision core ([`plan_schema`]) is pure (string-in, value-out) so the
//! add/update/no-op verdict, vendor formatting, and versioned `<VER>` filename
//! derivation are all unit-testable without git or network. The I/O shell
//! ([`run_publish`]) reads the synced upstream catalog, applies the planned
//! splices/writes, and opens the PR.

mod execute;
mod plan;

pub(crate) use execute::*;
pub(crate) use plan::*;

#[cfg(test)]
mod tests;
