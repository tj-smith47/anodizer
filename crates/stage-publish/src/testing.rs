//! Shared in-crate test doubles for publisher dispatch tests.
//!
//! Gated as `#[cfg(test)] pub(crate) mod testing;` in `lib.rs` so it's
//! visible to every test module in the crate but never compiled into the
//! library. The dispatch tests originally defined `FakePublisher` /
//! `FakeOutcome` privately; promoting them here lets the per-publisher
//! migrations reuse the same double without re-rolling one each time.

use anodizer_core::context::Context;
use anodizer_core::{PublishEvidence, Publisher, PublisherGroup};

/// Drives [`FakePublisher::run`].
pub enum FakeOutcome {
    Succeed,
    Fail(String),
}

/// Minimal [`Publisher`] implementation that records its identity and
/// returns a predetermined [`FakeOutcome`] from `run`.
pub struct FakePublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub outcome: FakeOutcome,
}

impl Publisher for FakePublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        match &self.outcome {
            FakeOutcome::Succeed => Ok(PublishEvidence::new(self.name.clone())),
            FakeOutcome::Fail(msg) => anyhow::bail!("{}", msg),
        }
    }
}

/// Convenience constructor returning the boxed-trait-object shape the
/// dispatcher consumes.
pub fn fake(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
    })
}
