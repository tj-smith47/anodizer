//! Publisher registry — single source of truth for which publishers run.
//!
//! [`configured_publishers`] walks the active [`Context`] and instantiates
//! a `Box<dyn Publisher>` for each configured publisher. The returned slice
//! is what [`crate::dispatch::dispatch`] iterates over.
//!
//! The registry starts empty and is populated incrementally by the
//! per-publisher migration tasks. The existing macro-driven
//! `PublishStage::run` body continues to dispatch publishers until those
//! migrations are complete; this module + [`crate::dispatch`] live
//! alongside it and are exercised only by tests until the swap lands.

use anodizer_core::context::Context;
use anodizer_core::{Publisher, PublisherGroup};

/// Returns the publishers configured for this release run.
///
/// Walks `ctx.config.publish` and instantiates a `Box<dyn Publisher>` for
/// each configured publisher. The returned slice is the single source of
/// truth that [`crate::dispatch::dispatch`] iterates.
///
/// Currently returns an empty `Vec`; per-publisher migration tasks add
/// entries one at a time.
pub fn configured_publishers(_ctx: &Context) -> Vec<Box<dyn Publisher>> {
    // Populated incrementally by per-publisher migrations.
    Vec::new()
}

/// Group dispatch order: Assets first (uploadable bytes, server-side
/// deletable), then Manager (package-manager state, also reversible), then
/// Submitter (irreversible / moderation-locked: chocolatey, winget, krew).
///
/// The Submitter group runs last so its irreversible publishes can be
/// gated on the success of every reversible publisher that came before
/// it. See [`crate::dispatch::dispatch`] for the gate mechanics.
pub const fn group_dispatch_order() -> [PublisherGroup; 3] {
    [
        PublisherGroup::Assets,
        PublisherGroup::Manager,
        PublisherGroup::Submitter,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_publishers_is_empty_until_migrations_land() {
        let ctx = Context::test_fixture();
        let publishers = configured_publishers(&ctx);
        assert!(
            publishers.is_empty(),
            "registry should remain empty until per-publisher migrations populate it"
        );
    }

    #[test]
    fn group_dispatch_order_is_assets_manager_submitter() {
        assert_eq!(
            group_dispatch_order(),
            [
                PublisherGroup::Assets,
                PublisherGroup::Manager,
                PublisherGroup::Submitter,
            ]
        );
    }
}
