//! SchemaStore publisher: registers a tool's JSON Schema(s) on
//! [SchemaStore](https://www.schemastore.org/) via a pull request against a
//! fork of `SchemaStore/schemastore`, plus the pure helpers (slug, description
//! validation, catalog-entry construction, JSON vendor formatting) it builds on.
//!
//! This module is the thin publisher home: the [`SchemastorePublisher`] struct,
//! its `impl Publisher`, and the shared [`entry_label`] message prefix. The
//! substantive logic lives in submodules — `preflight` (self-checks),
//! `catalog`/`scan`/`manifest` (pure builders) — so each can grow independently.

pub(crate) mod catalog;
pub(crate) mod manifest;
pub(crate) mod preflight;
pub(crate) mod publish;
pub(crate) mod rollback;
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

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let cfg = &ctx.config.schemastore;
        let globally_inactive = crate::publisher_helpers::entry_inactive(
            ctx,
            cfg.skip.as_ref(),
            None,
            cfg.if_condition.as_deref(),
        );
        let any_schema_active = cfg.schemas.iter().any(|s| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                s.skip.as_ref(),
                None,
                s.if_condition.as_deref(),
            )
        });
        if globally_inactive || !any_schema_active {
            return Vec::new();
        }
        crate::publisher_helpers::git_repo_requirements(
            ctx,
            ctx.config.schemastore.repository.as_ref(),
            Some("SCHEMASTORE_TOKEN"),
        )
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
/// Delegates to [`publish::run_publish`], which resolves the effective schemas,
/// builds the desired catalog entries, and (on the real path) opens one PR
/// against a fork of `SchemaStore/schemastore` carrying every registration.
fn run_publish(ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
    publish::run_publish(ctx)
}

/// Roll back a SchemaStore publish given its evidence.
///
/// Delegates to [`rollback::rollback_publish`], which closes the registration
/// PR(s) [`run_publish`] opened against `SchemaStore/schemastore` (best-effort,
/// mirroring krew's PR-close rollback).
fn rollback_publish(ctx: &mut Context, evidence: &PublishEvidence) -> anyhow::Result<()> {
    rollback::rollback_publish(ctx, evidence)
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

    #[test]
    fn rollback_with_no_targets_is_noop_warning() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let ev = PublishEvidence::new("schemastore");
        let p = SchemastorePublisher::new();
        assert!(p.rollback(&mut ctx, &ev).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("schemastore")
                && m.contains("PR targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }
}
