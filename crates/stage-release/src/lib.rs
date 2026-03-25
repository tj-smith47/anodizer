use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct ReleaseStage;

impl Stage for ReleaseStage {
    fn name(&self) -> &str { "release" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
