//! SchemaStore publisher: registers a tool's JSON Schema(s) on
//! [SchemaStore](https://www.schemastore.org/) via a pull request against a
//! fork of `SchemaStore/schemastore`, plus the pure helpers (slug, description
//! validation, catalog-entry construction, JSON vendor formatting) it builds on.
//!
//! This module is the thin publisher home: the [`SchemastorePublisher`] struct,
//! its `impl Publisher`, and the shared [`entry_label`] message prefix. The
//! substantive logic lives in submodules — `preflight` (self-checks),
//! `catalog`/`scan`/`manifest` (pure builders) — so each can grow independently.

// Interim `dead_code` for the whole module: the `simple_publisher!`-generated
// struct, its `impl Publisher`, and the run/rollback stubs have no production
// call site until the publisher registry constructs `SchemastorePublisher`. A
// per-item `#[allow]` cannot reach inside the macro expansion (the struct +
// `with_required` keep warning), so the allow is module-scoped here. Task 12
// wires the registry and removes it.
#![allow(dead_code)]

pub(crate) mod catalog;
pub(crate) mod manifest;
pub(crate) mod preflight;
pub(crate) mod scan;

#[cfg(test)]
mod tests;

use anodizer_core::context::Context;
use anodizer_core::{PreflightCheck, PublishEvidence, PublisherGroup};

/// The shared prefix for every operator-facing SchemaStore message about a
/// single schema entry: `` schemastore: schema `<name>` ``. Callers append the
/// specific cause (`": cannot read schema_file ..."`). Centralized here so the
/// prefix cannot drift across `preflight` and the (future) publish path.
pub(crate) fn entry_label(name: &str) -> String {
    format!("schemastore: schema `{name}`")
}

// Manager group: like krew/homebrew/scoop this pushes to a long-lived
// community index whose nightly clobber is disruptive, so `skips_on_nightly`
// is true. `required` defaults false so a release still succeeds if the
// registration PR cannot be opened; the per-entry config `required` overrides
// it.
simple_publisher!(
    SchemastorePublisher,
    "schemastore",
    PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

impl anodizer_core::Publisher for SchemastorePublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn preflight(&self, ctx: &Context) -> anyhow::Result<PreflightCheck> {
        preflight::preflight_checks(ctx)
    }
    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        run_publish(ctx)
    }
    fn rollback(&self, ctx: &mut Context, evidence: &PublishEvidence) -> anyhow::Result<()> {
        rollback_publish(ctx, evidence)
    }
}

/// Run the SchemaStore publish, returning evidence of what was registered.
///
/// Currently a no-op that returns empty evidence: the catalog-splice and PR
/// pipeline is built on the pure helpers in `catalog`/`scan`/`manifest` but
/// not yet driven from here.
fn run_publish(_ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
    Ok(PublishEvidence::new("schemastore"))
}

/// Roll back a SchemaStore publish given its evidence. Currently a no-op: the
/// PR-revert path has no recorded targets to act on yet.
fn rollback_publish(_ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{Publisher, PublisherGroup};

    #[test]
    fn entry_label_wraps_name_in_schema_prefix() {
        assert_eq!(entry_label("Anodizer"), "schemastore: schema `Anodizer`");
    }

    #[test]
    fn publisher_identity_is_manager_group_not_required_by_default() {
        let p = SchemastorePublisher::new();
        assert_eq!(p.name(), "schemastore");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert!(p.skips_on_nightly());
    }

    #[test]
    fn publisher_declares_rollback_scope() {
        let p = SchemastorePublisher::new();
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN pull_request:write")
        );
    }

    #[test]
    fn run_and_rollback_are_nonpanicking_stubs() {
        let mut ctx = TestContextBuilder::new().build();
        let p = SchemastorePublisher::new();
        let ev = p.run(&mut ctx).expect("run stub ok");
        assert_eq!(ev.publisher, "schemastore");
        assert!(p.rollback(&mut ctx, &ev).is_ok());
    }
}
