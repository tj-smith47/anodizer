use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct AnnounceStage;

impl Stage for AnnounceStage {
    fn name(&self) -> &str { "announce" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
